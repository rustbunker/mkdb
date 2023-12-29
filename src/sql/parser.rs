use core::iter::Peekable;

use super::{
    statement::{
        BinaryOperator, Column, Constraint, Create, DataType, Drop, Expression, Statement, Value,
    },
    token::{Keyword, Token},
    tokenizer::{self, Location, TokenWithLocation, Tokenizer, TokenizerError},
};

#[derive(Debug, PartialEq)]
pub(crate) struct ParserError {
    message: String,
    location: Location,
}

impl ParserError {
    fn new(message: impl Into<String>, location: Location) -> Self {
        Self {
            message: message.into(),
            location,
        }
    }
}

impl From<TokenizerError> for ParserError {
    fn from(err: TokenizerError) -> Self {
        Self {
            message: format!("syntax error: {}", err.message),
            location: err.location,
        }
    }
}

pub(crate) type ParseResult<T> = Result<T, ParserError>;

/// TODP (Top-Down Operator Precedence) recursive descent parser. See this
/// [tutorial] for an introduction to the algorithms used here and see also the
/// [sqlparser] Github repo for a more complete and robust SQL parser written in
/// Rust. This one is simply a toy parser implemented for the sake of it.
///
/// [tutorial]: https://eli.thegreenplace.net/2010/01/02/top-down-operator-precedence-parsing
/// [sqlparser]: https://github.com/sqlparser-rs/sqlparser-rs
pub(crate) struct Parser<'i> {
    /// [`Token`] peekable iterator.
    tokenizer: Peekable<tokenizer::IntoIter<'i>>,
    /// Location of the last token we've consumed from the iterator.
    location: Location,
}

impl<'i> Parser<'i> {
    /// Creates a new parser for the given `input` string.
    pub fn new(input: &'i str) -> Self {
        Self {
            tokenizer: Tokenizer::new(input).into_iter().peekable(),
            location: Location::default(),
        }
    }

    /// Attempts to parse the `input` string into a list of [`Statement`]
    /// instances.
    pub fn try_parse(&mut self) -> ParseResult<Vec<Statement>> {
        let mut statements = Vec::new();

        loop {
            match self.peek_token() {
                Some(Ok(Token::Eof)) | None => return Ok(statements),
                _ => statements.push(self.parse_statement()?),
            }
        }
    }

    /// Parses a single SQL statement in the input string. If the statement
    /// terminator is not found then it returns [`Err`].
    fn parse_statement(&mut self) -> ParseResult<Statement> {
        let Token::Keyword(keyword) = self.next_token()? else {
            return Err(self.error(format!(
                "unexpected initial token. Statements must start with one of the supported keywords."
            )));
        };

        let statement = match keyword {
            Keyword::Select => {
                let columns = self.parse_comma_separated_expressions()?;
                self.expect_keyword(Keyword::From)?;

                let (from, r#where) = self.parse_from_and_optional_where()?;

                Ok(Statement::Select {
                    columns,
                    from,
                    r#where,
                })
            }

            Keyword::Create => {
                let keyword = self.expect_one_of(&[Keyword::Database, Keyword::Table])?;
                let identifier = self.parse_identifier()?;

                Ok(Statement::Create(match keyword {
                    Keyword::Database => Create::Database(identifier),

                    Keyword::Table => Create::Table {
                        name: identifier,
                        columns: self.parse_schema()?,
                    },

                    _ => unreachable!(),
                }))
            }

            Keyword::Update => {
                let table = self.parse_identifier()?;
                self.expect_keyword(Keyword::Set)?;

                let columns = self.parse_comma_separated_expressions()?;
                let r#where = self.parse_optional_where()?;

                Ok(Statement::Update {
                    table,
                    columns,
                    r#where,
                })
            }

            Keyword::Insert => {
                self.expect_keyword(Keyword::Into)?;
                let into = self.parse_identifier()?;
                let columns = self.parse_identifier_list()?;

                self.expect_keyword(Keyword::Values)?;
                let values = self.parse_comma_separated_expressions()?;

                Ok(Statement::Insert {
                    into,
                    columns,
                    values,
                })
            }

            Keyword::Delete => {
                self.expect_keyword(Keyword::From)?;
                let (from, r#where) = self.parse_from_and_optional_where()?;

                Ok(Statement::Delete { from, r#where })
            }

            Keyword::Drop => {
                let keyword = self.expect_one_of(&[Keyword::Database, Keyword::Table])?;
                let identifier = self.parse_identifier()?;

                Ok(Statement::Drop(match keyword {
                    Keyword::Database => Drop::Database(identifier),
                    Keyword::Table => Drop::Table(identifier),
                    _ => unreachable!(),
                }))
            }

            Keyword::None => Err(self.error("expected SQL statement")),

            _ => Err(self.error(format!("unexpected initial statement keyword: {keyword}"))),
        };

        if statement.is_ok() {
            self.expect_semicolon()?;
        }

        statement
    }

    /// TODP recursive descent consists of 3 functions that call each other
    /// recursively:
    ///
    /// - [`Self::parse_expr`]
    /// - [`Self::parse_prefix`]
    /// - [`Self::parse_infix`]
    ///
    /// This one simply initiates the process, see the others for details and
    /// see the [tutorial] mentioned above to understand how the algorithm
    /// works.
    ///
    /// [tutorial]: https://eli.thegreenplace.net/2010/01/02/top-down-operator-precedence-parsing
    fn parse_expression(&mut self) -> ParseResult<Expression> {
        self.parse_expr(0)
    }

    /// Main TODP loop.
    fn parse_expr(&mut self, precedence: u8) -> ParseResult<Expression> {
        let mut expr = self.parse_prefix()?;
        let mut next_precedence = self.get_next_precedence();

        while precedence < next_precedence {
            expr = self.parse_infix(expr, next_precedence)?;
            next_precedence = self.get_next_precedence();
        }

        Ok(expr)
    }

    /// Parses the beginning of an expression.
    fn parse_prefix(&mut self) -> ParseResult<Expression> {
        match self.next_token()? {
            Token::Identifier(ident) => Ok(Expression::Identifier(ident)),
            Token::Mul => Ok(Expression::Wildcard),
            Token::Number(num) => Ok(Expression::Value(Value::Number(num))),
            Token::String(string) => Ok(Expression::Value(Value::String(string))),

            Token::LeftParen => {
                let expr = self.parse_expression()?;
                self.expect_token(Token::RightParen)?;
                Ok(expr)
            }

            unexpected => Err(self.error(format!(
                "expected an identifier, raw value or opening parenthesis. Got '{unexpected}' instead",
            ))),
        }
    }

    /// Parses an infix expression in the form of
    /// (left expr | operator | right expr).
    fn parse_infix(&mut self, left: Expression, precedence: u8) -> ParseResult<Expression> {
        let token = self.next_token()?;

        let operator = match token {
            Token::Plus => BinaryOperator::Plus,
            Token::Minus => BinaryOperator::Minus,
            Token::Div => BinaryOperator::Div,
            Token::Mul => BinaryOperator::Mul,
            Token::Eq => BinaryOperator::Eq,
            Token::Neq => BinaryOperator::Neq,
            Token::Gt => BinaryOperator::Gt,
            Token::GtEq => BinaryOperator::GtEq,
            Token::Lt => BinaryOperator::Lt,
            Token::LtEq => BinaryOperator::LtEq,
            Token::Keyword(Keyword::And) => BinaryOperator::And,
            Token::Keyword(Keyword::Or) => BinaryOperator::Or,

            _ => Err(self.error(format!(
                "expected an operator: [+, -, *, /, =, !=, <, >, <=, >=, AND, OR]. Got {token} instead"
            )))?,
        };

        Ok(Expression::BinaryOperation {
            left: Box::new(left),
            operator,
            right: Box::new(self.parse_expr(precedence)?),
        })
    }

    /// Returns the precedence value of the next operator in the stream.
    fn get_next_precedence(&mut self) -> u8 {
        let Some(Ok(token)) = self.peek_token() else {
            return 0;
        };

        match token {
            Token::Keyword(Keyword::Or) => 5,
            Token::Keyword(Keyword::And) => 10,
            Token::Eq | Token::Neq | Token::Gt | Token::GtEq | Token::Lt | Token::LtEq => 20,
            Token::Plus | Token::Minus => 30,
            Token::Mul | Token::Div => 40,
            _ => 0,
        }
    }

    /// Parses a column definition for `CREATE TABLE` statements.
    fn parse_column(&mut self) -> ParseResult<Column> {
        let name = self.parse_identifier()?;

        let Token::Keyword(keyword) = self.next_token()? else {
            return Err(self.error("expected data type"));
        };

        let data_type = match keyword {
            Keyword::Int => DataType::Int,

            Keyword::Varchar => {
                self.expect_token(Token::LeftParen)?;

                let length = match self.next_token()? {
                    Token::Number(num) => num
                        .parse()
                        .map_err(|_| self.error("incorrect varchar length"))?,
                    _ => Err(self.error("expected varchar length"))?,
                };

                self.expect_token(Token::RightParen)?;
                DataType::Varchar(length)
            }

            unexpected => Err(self.error(format!("unexpected keyword {unexpected}")))?,
        };

        let constraint = match self.consume_one_of(&[Keyword::Primary, Keyword::Unique]) {
            Keyword::Primary => {
                self.expect_keyword(Keyword::Key)?;
                Some(Constraint::PrimaryKey)
            }

            Keyword::Unique => Some(Constraint::Unique),

            Keyword::None => None,

            _ => unreachable!(),
        };

        Ok(Column {
            name,
            data_type,
            constraint,
        })
    }

    /// Takes a `subparser` as input and calls it after every instance of
    /// [`Token::Comma`].
    fn parse_comma_separated<T>(
        &mut self,
        mut subparser: impl FnMut(&mut Self) -> ParseResult<T>,
        required_parenthesis: bool,
    ) -> ParseResult<Vec<T>> {
        let left_paren = self.consume_optional_token(Token::LeftParen);

        if required_parenthesis && !left_paren {
            return Err(self.error("opening parenthesis is required"));
        }

        let mut results = vec![subparser(self)?];
        while self.consume_optional_token(Token::Comma) {
            results.push(subparser(self)?);
        }

        if left_paren {
            self.expect_token(Token::RightParen)?;
        }

        Ok(results)
    }

    /// Used to parse the expressions after `SELECT`, `WHERE`, `SET` or `VALUES`.
    fn parse_comma_separated_expressions(&mut self) -> ParseResult<Vec<Expression>> {
        self.parse_comma_separated(Self::parse_expression, false)
    }

    /// Used to parse `CREATE TABLE` column definitions.
    fn parse_schema(&mut self) -> ParseResult<Vec<Column>> {
        self.parse_comma_separated(Self::parse_column, true)
    }

    /// Expects a list of identifiers, not complete expressions.
    fn parse_identifier_list(&mut self) -> ParseResult<Vec<String>> {
        self.parse_comma_separated(Self::parse_identifier, true)
    }

    /// Parses the next identifier in the stream or fails if it's not an
    /// identifier.
    fn parse_identifier(&mut self) -> ParseResult<String> {
        self.next_token().and_then(|token| match token {
            Token::Identifier(ident) => Ok(ident),
            _ => Err(self.error(format!("expected identifier. Got {token} instead"))),
        })
    }

    /// Parses the entire `WHERE` clause if the next token is [`Keyword::Where`].
    fn parse_optional_where(&mut self) -> ParseResult<Option<Expression>> {
        if self.consume_optional_keyword(Keyword::Where) {
            Ok(Some(self.parse_expression()?))
        } else {
            Ok(None)
        }
    }

    /// These statements all have a `FROM` clause and an optional `WHERE`
    /// clause:
    ///
    /// ```sql
    /// SELECT * FROM table WHERE condition;
    /// UPDATE table SET column = "value" WHERE condition;
    /// DELETE FROM table WHERE condition;
    /// ```
    fn parse_from_and_optional_where(&mut self) -> ParseResult<(String, Option<Expression>)> {
        let from = self.parse_identifier()?;
        let r#where = self.parse_optional_where()?;

        Ok((from, r#where))
    }

    /// Same as [`Self::expect_token`] but takes [`Keyword`] variants instead.
    fn expect_keyword(&mut self, expected: Keyword) -> ParseResult<Keyword> {
        self.expect_token(Token::Keyword(expected))
            .map(|_| expected)
    }

    /// SQL statements must end with `;`, or [`Token::SemiColon`] in this
    /// context.
    fn expect_semicolon(&mut self) -> ParseResult<Token> {
        self.expect_token(Token::SemiColon).map_err(|_| {
            self.error(format!(
                "missing '{}' statement terminator",
                Token::SemiColon
            ))
        })
    }

    /// Automatically fails if the `expected` token is not the next one in the
    /// stream (after whitespaces). If it is, it will be returned back.
    fn expect_token(&mut self, expected: Token) -> ParseResult<Token> {
        self.next_token().and_then(|token| {
            if token == expected {
                Ok(token)
            } else {
                Err(self.error(format!("expected token {expected}. Got {token} instead")))
            }
        })
    }

    /// Automatically fails if the next token does not match one of the given
    /// `keywords`. If it does, then the keyword that matched is returned back.
    fn expect_one_of(&mut self, keywords: &[Keyword]) -> ParseResult<Keyword> {
        match self.consume_one_of(keywords) {
            Keyword::None => {
                let token = self.next_token()?;
                Err(self.error(format!("expected one of {keywords:?}. Got {token} instead")))
            }
            keyword => Ok(keyword),
        }
    }

    /// Consumes all the tokens before and including the given `optional`
    /// keyword. If the keyword is not found, only whitespaces are consumed.
    fn consume_optional_keyword(&mut self, optional: Keyword) -> bool {
        self.consume_optional_token(Token::Keyword(optional))
    }

    /// If the next token in the stream matches the given `optional` token, then
    /// this function consumes the token and returns `true`. Otherwise the token
    /// will not be consumed and the tokenizer will still be pointing at it.
    fn consume_optional_token(&mut self, optional: Token) -> bool {
        match self.peek_token() {
            Some(Ok(token)) if token == &optional => {
                let _ = self.next_token();
                true
            }
            _ => false,
        }
    }

    /// Consumes the next token in the stream only if it matches one of the
    /// given `keywords`. If so, the matched [`Keyword`] variant is returned.
    /// Otherwise returns [`Keyword::None`].
    fn consume_one_of(&mut self, keywords: &[Keyword]) -> Keyword {
        *keywords
            .into_iter()
            .find(|keyword| self.consume_optional_keyword(**keyword))
            .unwrap_or(&Keyword::None)
    }

    /// Builds an instance of [`ParserError`] giving it the current
    /// [`Self::location`].
    fn error(&self, message: impl Into<String>) -> ParserError {
        ParserError {
            message: message.into(),
            location: self.location,
        }
    }

    /// Skips all instances of [`Token::Whitespace`] in the stream.
    fn skip_white_spaces(&mut self) {
        while let Some(Ok(TokenWithLocation {
            token: Token::Whitespace(_),
            ..
        })) = self.tokenizer.peek()
        {
            let _ = self.tokenizer.next();
        }
    }

    /// Skips all instances of [`Token::Whitespace`] and returns the next
    /// relevant [`Token`]. This function doesn't return [`Option`] because
    /// it's used in all cases to expect some token. If we dont' expect any
    /// more tokens (for example, after we've found [`Token::SemiColon`]) then
    /// we just won't call this function at al.
    fn next_token(&mut self) -> ParseResult<Token> {
        self.skip_white_spaces();

        let token = self.tokenizer.next().map(|result| {
            result.map(|TokenWithLocation { token, location }| {
                self.location = location;
                token
            })
        });

        match token {
            None => Err(self.error("unexpected EOF")),
            Some(result) => Ok(result?),
        }
    }

    /// Returns a reference to the next relevant [`Token`] after whitespaces
    /// without consuming it.
    fn peek_token(&mut self) -> Option<Result<&Token, &TokenizerError>> {
        self.skip_white_spaces();
        self.tokenizer
            .peek()
            .map(|result| result.as_ref().map(TokenWithLocation::token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_select() {
        let sql = "SELECT id, name FROM users;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Select {
                columns: vec![
                    Expression::Identifier("id".into()),
                    Expression::Identifier("name".into())
                ],
                from: "users".into(),
                r#where: None
            })
        )
    }

    #[test]
    fn parse_select_wildcard() {
        let sql = "SELECT * FROM users;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Select {
                columns: vec![Expression::Wildcard],
                from: "users".into(),
                r#where: None
            })
        )
    }

    #[test]
    fn parse_select_where() {
        let sql = "SELECT id, price, discount FROM products WHERE price >= 100;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Select {
                columns: vec![
                    Expression::Identifier("id".into()),
                    Expression::Identifier("price".into()),
                    Expression::Identifier("discount".into())
                ],
                from: "products".into(),
                r#where: Some(Expression::BinaryOperation {
                    left: Box::new(Expression::Identifier("price".into())),
                    operator: BinaryOperator::GtEq,
                    right: Box::new(Expression::Value(Value::Number("100".into())))
                })
            })
        )
    }

    #[test]
    fn parse_select_with_expressions() {
        let sql = r#"
            SELECT id, price, discount, price * discount / 100
            FROM products
            WHERE 100 <= price AND price < 1000 OR discount < 10 + (2 * 20);
        "#;

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Select {
                columns: vec![
                    Expression::Identifier("id".into()),
                    Expression::Identifier("price".into()),
                    Expression::Identifier("discount".into()),
                    Expression::BinaryOperation {
                        left: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Identifier("price".into())),
                            operator: BinaryOperator::Mul,
                            right: Box::new(Expression::Identifier("discount".into())),
                        }),
                        operator: BinaryOperator::Div,
                        right: Box::new(Expression::Value(Value::Number("100".into()))),
                    }
                ],
                from: "products".into(),
                r#where: Some(Expression::BinaryOperation {
                    left: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Value(Value::Number("100".into()))),
                            operator: BinaryOperator::LtEq,
                            right: Box::new(Expression::Identifier("price".into())),
                        }),
                        operator: BinaryOperator::And,
                        right: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Identifier("price".into())),
                            operator: BinaryOperator::Lt,
                            right: Box::new(Expression::Value(Value::Number("1000".into()))),
                        }),
                    }),
                    operator: BinaryOperator::Or,
                    right: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("discount".into())),
                        operator: BinaryOperator::Lt,
                        right: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Value(Value::Number("10".into()))),
                            operator: BinaryOperator::Plus,
                            right: Box::new(Expression::BinaryOperation {
                                left: Box::new(Expression::Value(Value::Number("2".into()))),
                                operator: BinaryOperator::Mul,
                                right: Box::new(Expression::Value(Value::Number("20".into()))),
                            })
                        }),
                    })
                })
            })
        )
    }

    #[test]
    fn parse_create_database() {
        let sql = "CREATE DATABASE test;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Create(Create::Database("test".into())))
        )
    }

    #[test]
    fn parse_create_table() {
        let sql = r#"
            CREATE TABLE users (
                id INT PRIMARY KEY,
                name VARCHAR(255),
                email VARCHAR(255) UNIQUE
            );
        "#;

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Create(Create::Table {
                name: "users".into(),
                columns: vec![
                    Column {
                        name: "id".into(),
                        data_type: DataType::Int,
                        constraint: Some(Constraint::PrimaryKey),
                    },
                    Column {
                        name: "name".into(),
                        data_type: DataType::Varchar(255),
                        constraint: None,
                    },
                    Column {
                        name: "email".into(),
                        data_type: DataType::Varchar(255),
                        constraint: Some(Constraint::Unique),
                    },
                ]
            }))
        )
    }

    #[test]
    fn parse_simple_update() {
        let sql = "UPDATE users SET is_admin = 1;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Update {
                table: "users".into(),
                columns: vec![Expression::BinaryOperation {
                    left: Box::new(Expression::Identifier("is_admin".into())),
                    operator: BinaryOperator::Eq,
                    right: Box::new(Expression::Value(Value::Number("1".into()))),
                }],
                r#where: None,
            })
        )
    }

    #[test]
    fn parse_update_where() {
        let sql = r#"
            UPDATE products
            SET price = price - 10, discount = 15, stock = 10
            WHERE price > 100;
        "#;

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Update {
                table: "products".into(),
                columns: vec![
                    Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("price".into())),
                        operator: BinaryOperator::Eq,
                        right: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Identifier("price".into())),
                            operator: BinaryOperator::Minus,
                            right: Box::new(Expression::Value(Value::Number("10".into()))),
                        }),
                    },
                    Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("discount".into())),
                        operator: BinaryOperator::Eq,
                        right: Box::new(Expression::Value(Value::Number("15".into()))),
                    },
                    Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("stock".into())),
                        operator: BinaryOperator::Eq,
                        right: Box::new(Expression::Value(Value::Number("10".into()))),
                    }
                ],
                r#where: Some(Expression::BinaryOperation {
                    left: Box::new(Expression::Identifier("price".into())),
                    operator: BinaryOperator::Gt,
                    right: Box::new(Expression::Value(Value::Number("100".into()))),
                })
            })
        )
    }

    #[test]
    fn parse_delete_from() {
        let sql = "DELETE FROM products;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Delete {
                from: "products".into(),
                r#where: None
            })
        )
    }

    #[test]
    fn parse_delete_from_where() {
        let sql = "DELETE FROM products WHERE price > 5000;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Delete {
                from: "products".into(),
                r#where: Some(Expression::BinaryOperation {
                    left: Box::new(Expression::Identifier("price".into())),
                    operator: BinaryOperator::Gt,
                    right: Box::new(Expression::Value(Value::Number("5000".into()))),
                })
            })
        )
    }

    #[test]
    fn parse_insert_into() {
        let sql = r#"INSERT INTO users (id, name, email) VALUES (1, "Test", "test@test.com");"#;

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Insert {
                into: "users".into(),
                columns: ["id", "name", "email"].map(String::from).into(),
                values: vec![
                    Expression::Value(Value::Number("1".into())),
                    Expression::Value(Value::String("Test".into())),
                    Expression::Value(Value::String("test@test.com".into())),
                ]
            })
        );
    }

    #[test]
    fn parse_drop_database() {
        let sql = "DROP DATABASE test;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Drop(Drop::Database("test".into())))
        )
    }

    #[test]
    fn parse_drop_table() {
        let sql = "DROP TABLE test;";

        assert_eq!(
            Parser::new(sql).parse_statement(),
            Ok(Statement::Drop(Drop::Table("test".into())))
        )
    }

    #[test]
    fn parse_multiple_statements() {
        let sql = r#"
            DROP TABLE test;
            UPDATE users SET is_admin = 1;
            SELECT * FROM products;
        "#;

        assert_eq!(
            Parser::new(sql).try_parse(),
            Ok(vec![
                Statement::Drop(Drop::Table("test".into())),
                Statement::Update {
                    table: "users".into(),
                    columns: vec![Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("is_admin".into())),
                        operator: BinaryOperator::Eq,
                        right: Box::new(Expression::Value(Value::Number("1".into()))),
                    }],
                    r#where: None,
                },
                Statement::Select {
                    columns: vec![Expression::Wildcard],
                    from: "products".into(),
                    r#where: None,
                }
            ])
        )
    }

    #[test]
    fn arithmetic_operator_precedence() {
        let expr = "price * discount / 100 < 10 + 20 * 30";

        assert_eq!(
            Parser::new(expr).parse_expression(),
            Ok(Expression::BinaryOperation {
                left: Box::new(Expression::BinaryOperation {
                    left: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("price".into())),
                        operator: BinaryOperator::Mul,
                        right: Box::new(Expression::Identifier("discount".into())),
                    }),
                    operator: BinaryOperator::Div,
                    right: Box::new(Expression::Value(Value::Number("100".into()))),
                }),
                operator: BinaryOperator::Lt,
                right: Box::new(Expression::BinaryOperation {
                    left: Box::new(Expression::Value(Value::Number("10".into()))),
                    operator: BinaryOperator::Plus,
                    right: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::Value(Value::Number("20".into()))),
                        operator: BinaryOperator::Mul,
                        right: Box::new(Expression::Value(Value::Number("30".into()))),
                    })
                })
            })
        )
    }

    #[test]
    fn nested_arithmetic_precedence() {
        let expr = "price * discount >= 10 - (20 + 50) / (2 * (4 + (1 - 1)))";

        assert_eq!(
            Parser::new(expr).parse_expression(),
            Ok(Expression::BinaryOperation {
                left: Box::new(Expression::BinaryOperation {
                    left: Box::new(Expression::Identifier("price".into())),
                    operator: BinaryOperator::Mul,
                    right: Box::new(Expression::Identifier("discount".into())),
                }),
                operator: BinaryOperator::GtEq,
                right: Box::new(Expression::BinaryOperation {
                    left: Box::new(Expression::Value(Value::Number("10".into()))),
                    operator: BinaryOperator::Minus,
                    right: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Value(Value::Number("20".into()))),
                            operator: BinaryOperator::Plus,
                            right: Box::new(Expression::Value(Value::Number("50".into()))),
                        }),
                        operator: BinaryOperator::Div,
                        right: Box::new(Expression::BinaryOperation {
                            left: Box::new(Expression::Value(Value::Number("2".into()))),
                            operator: BinaryOperator::Mul,
                            right: Box::new(Expression::BinaryOperation {
                                left: Box::new(Expression::Value(Value::Number("4".into()))),
                                operator: BinaryOperator::Plus,
                                right: Box::new(Expression::BinaryOperation {
                                    left: Box::new(Expression::Value(Value::Number("1".into()))),
                                    operator: BinaryOperator::Minus,
                                    right: Box::new(Expression::Value(Value::Number("1".into()))),
                                })
                            })
                        })
                    })
                })
            })
        )
    }

    #[test]
    fn and_or_operators_precedence() {
        let expr = "100 <= price AND price <= 200 OR price > 1000";

        assert_eq!(
            Parser::new(expr).parse_expression(),
            Ok(Expression::BinaryOperation {
                left: Box::new(Expression::BinaryOperation {
                    left: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::Value(Value::Number("100".into()))),
                        operator: BinaryOperator::LtEq,
                        right: Box::new(Expression::Identifier("price".into())),
                    }),
                    operator: BinaryOperator::And,
                    right: Box::new(Expression::BinaryOperation {
                        left: Box::new(Expression::Identifier("price".into())),
                        operator: BinaryOperator::LtEq,
                        right: Box::new(Expression::Value(Value::Number("200".into()))),
                    }),
                }),
                operator: BinaryOperator::Or,
                right: Box::new(Expression::BinaryOperation {
                    left: Box::new(Expression::Identifier("price".into())),
                    operator: BinaryOperator::Gt,
                    right: Box::new(Expression::Value(Value::Number("1000".into()))),
                })
            })
        )
    }
}