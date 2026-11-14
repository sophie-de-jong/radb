//! The SQL parser: a hand-written recursive-descent parser over
//! [`Scanner`](super::tokenizer::Scanner).
//!
//! One method per nonterminal, in the style of Bonus Project 1. `parse_program`
//! loops over statements, `parse_query` folds set operators left to right,
//! `parse_select_block` reads the clauses in §2's order, and conditions are
//! their own descent (`or` → `and` → `not` → comparison). The same descent
//! parses the JSON string form of a [`Condition`] through the private
//! `parse_condition` codec.
//! There are no regular expressions and no parser generator.
//!
//! The parser is purely syntactic. It does not know a catalog, so a name is
//! kept exactly as written and no error here is about meaning; every name,
//! type and aggregate problem is reported later, by the resolver.
//!
//! Where this grammar differs from the §2 sketch it is documented in
//! `docs/M3/GRAMMAR_SQL.md`; the two deliberate differences are that a
//! qualified name is two tokens here (the scanner keeps `e.name` apart) and
//! that an aggregate may appear as a condition operand so `HAVING COUNT(*) > 5`
//! can be parsed and then checked.

use super::ast::CompareOp;
use super::tokenizer::Position;
use crate::Value;
use crate::exec::Direction;
use crate::exec::plan::Aggregate;
use crate::schema::ColumnType;

use super::ast::*;
use super::error::SqlError;
use super::tokenizer::{Keyword, Scanner, Token, TokenKind};

/// Parse every statement in a SQL file or query string.
///
/// Statements are separated by `;`; empty statements, and a trailing `;`, are
/// allowed, so a file of nothing but whitespace and semicolons parses to no
/// statements at all.
///
/// Errors: the first lexical or syntax [`SqlError`].
pub(crate) fn parse_program(input: &str) -> Result<Vec<Statement>, SqlError> {
    let mut parser = Parser::new(input)?;
    let mut statements = Vec::new();
    while let Some(statement) = parser.next_statement()? {
        statements.push(statement);
    }
    Ok(statements)
}

impl std::str::FromStr for Condition {
    type Err = SqlError;

    /// Parse the JSON wire form of a condition.
    ///
    /// The string is only used at the plan boundary; plans themselves carry a
    /// [`Condition`]. The grammar is the SQL parser's own condition grammar.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut parser = Parser::new(input)?;
        let condition = parser.parse_condition()?;
        if !matches!(parser.peek(), TokenKind::Eof) {
            return Err(parser.unexpected("end of input"));
        }
        Ok(condition)
    }
}

impl<'de> serde::Deserialize<'de> for Condition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for Condition {
    /// A condition is a string in the shared condition language, so a plan's
    /// JSON is exactly the shape the M2 and M3 specifications show.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// A streaming recursive-descent parser over a [`Scanner`], with one token of
/// lookahead.
pub(crate) struct Parser<'a> {
    scanner: Scanner<'a>,
    current: Token,
}

impl<'a> Parser<'a> {
    /// A parser for `input`, with the first token already read.
    pub fn new(input: &'a str) -> Result<Self, SqlError> {
        let mut scanner = Scanner::new(input);
        let current = scanner.next_token()?;
        Ok(Parser { scanner, current })
    }

    /// The position of the current token.
    pub fn at(&self) -> Position {
        self.current.at
    }

    /// The kind of the current token.
    fn peek(&self) -> &TokenKind {
        &self.current.kind
    }

    /// Whether the current token is the keyword `kw`.
    fn at_keyword(&self, kw: Keyword) -> bool {
        self.peek().is_keyword(kw)
    }

    /// Consume the current token and read the next one.
    fn advance(&mut self) -> Result<(), SqlError> {
        self.current = self.scanner.next_token()?;
        Ok(())
    }

    /// Consume the current token if it is `kind`, reporting that it was
    /// expected otherwise.
    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<(), SqlError> {
        if self.peek() == kind {
            self.advance()
        } else {
            Err(self.unexpected(what))
        }
    }

    /// Consume the current token if it is the keyword `kw`, reporting that it
    /// was expected otherwise.
    fn expect_keyword(&mut self, kw: Keyword, what: &str) -> Result<(), SqlError> {
        if self.at_keyword(kw) {
            self.advance()
        } else {
            Err(self.unexpected(what))
        }
    }

    /// Consume the keyword `kw` if it is current, reporting whether it was.
    fn eat_keyword(&mut self, kw: Keyword) -> Result<bool, SqlError> {
        if self.at_keyword(kw) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// A syntax error saying `expected` belonged at the current token; end of
    /// input is special-cased in its wording.
    fn unexpected(&self, expected: &str) -> SqlError {
        let found = self.peek().describe();
        SqlError::syntax(
            format!("unexpected {found}, expected {expected}"),
            self.at(),
        )
    }

    /// Parse the next statement, or `None` at end of input.
    pub fn next_statement(&mut self) -> Result<Option<Statement>, SqlError> {
        // Skip any number of empty statements.
        while matches!(self.peek(), TokenKind::Semicolon) {
            self.advance()?;
        }
        if matches!(self.peek(), TokenKind::Eof) {
            return Ok(None);
        }
        let statement = self.parse_statement()?;
        if !matches!(self.peek(), TokenKind::Semicolon | TokenKind::Eof) {
            return Err(self.unexpected("';' or end of input"));
        }
        Ok(Some(statement))
    }

    // =================================================================
    // Statements
    // =================================================================

    /// `statement ::= create | insert | query`.
    fn parse_statement(&mut self) -> Result<Statement, SqlError> {
        if self.at_keyword(Keyword::Create) {
            Ok(Statement::Create(self.parse_create()?))
        } else if self.at_keyword(Keyword::Insert) {
            Ok(Statement::Insert(self.parse_insert()?))
        } else if self.at_keyword(Keyword::Select) || matches!(self.peek(), TokenKind::LParen) {
            Ok(Statement::Query(self.parse_query()?))
        } else {
            Err(self.unexpected("'CREATE', 'INSERT' or 'SELECT'"))
        }
    }

    /// `create ::= CREATE TABLE ident "(" coldef { "," coldef } ")"`.
    fn parse_create(&mut self) -> Result<Located<CreateTable>, SqlError> {
        let at = self.at();
        self.expect_keyword(Keyword::Create, "'CREATE'")?;
        self.expect_keyword(Keyword::Table, "'TABLE'")?;
        let name = self.parse_ident("a table name")?;
        self.expect(&TokenKind::LParen, "'('")?;
        let mut columns = vec![self.parse_coldef()?];
        while matches!(self.peek(), TokenKind::Comma) {
            self.advance()?;
            columns.push(self.parse_coldef()?);
        }
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Located::new(CreateTable { name, columns }, at))
    }

    /// `coldef ::= ident ( INT | VARCHAR "(" int ")" )`.
    fn parse_coldef(&mut self) -> Result<ColumnDef, SqlError> {
        let name = self.parse_ident("a column name")?;
        let ty = if self.at_keyword(Keyword::Int) {
            self.advance()?;
            ColumnType::Int
        } else if self.at_keyword(Keyword::Varchar) {
            self.advance()?;
            self.expect(&TokenKind::LParen, "'('")?;
            let length_at = self.at();
            let length = self.parse_non_negative_int("a VARCHAR length")?;
            self.expect(&TokenKind::RParen, "')'")?;
            let length = u8::try_from(length).map_err(|_| {
                SqlError::schema("VARCHAR length must be between 0 and 255", length_at)
            })?;
            ColumnType::Varchar(length)
        } else {
            return Err(self.unexpected("'INT' or 'VARCHAR'"));
        };
        Ok(ColumnDef { name, ty })
    }

    /// `insert ::= INSERT INTO ident VALUES "(" literal { "," literal } ")"`.
    fn parse_insert(&mut self) -> Result<Located<Insert>, SqlError> {
        let at = self.at();
        self.expect_keyword(Keyword::Insert, "'INSERT'")?;
        self.expect_keyword(Keyword::Into, "'INTO'")?;
        let table = self.parse_ident("a table name")?;
        self.expect_keyword(Keyword::Values, "'VALUES'")?;
        self.expect(&TokenKind::LParen, "'('")?;
        let mut values = vec![self.parse_literal()?];
        while matches!(self.peek(), TokenKind::Comma) {
            self.advance()?;
            values.push(self.parse_literal()?);
        }
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Located::new(Insert { table, values }, at))
    }

    /// `literal ::= int | string`.
    fn parse_literal(&mut self) -> Result<Located<Value>, SqlError> {
        match self.peek().clone() {
            TokenKind::Int(value) => {
                let at = self.at();
                self.advance()?;
                Ok(Located::new(Value::Int(value), at))
            }
            TokenKind::Str(value) => {
                let at = self.at();
                self.advance()?;
                Ok(Located::new(Value::Varchar(value), at))
            }
            _ => Err(self.unexpected("an integer or string literal")),
        }
    }

    // =================================================================
    // Queries
    // =================================================================

    /// `query ::= select_block { setop select_block }`, left-associative and
    /// all at one precedence level.
    fn parse_query(&mut self) -> Result<Query, SqlError> {
        let first = self.parse_select_block()?;
        let mut rest = Vec::new();
        loop {
            let op = if self.at_keyword(Keyword::Union) {
                self.advance()?;
                if self.eat_keyword(Keyword::All)? {
                    SetOp::UnionAll
                } else {
                    SetOp::Union
                }
            } else if self.at_keyword(Keyword::Intersect) {
                self.advance()?;
                SetOp::Intersect
            } else if self.at_keyword(Keyword::Except) {
                self.advance()?;
                SetOp::Except
            } else {
                break;
            };
            rest.push((op, self.parse_select_block()?));
        }
        Ok(Query { first, rest })
    }

    /// `select_block ::= SELECT ... | "(" query ")"`.
    fn parse_select_block(&mut self) -> Result<SelectBlock, SqlError> {
        if matches!(self.peek(), TokenKind::LParen) {
            self.advance()?;
            let query = self.parse_query()?;
            self.expect(&TokenKind::RParen, "')'")?;
            return Ok(SelectBlock::Sub(Box::new(query)));
        }
        Ok(SelectBlock::Select(self.parse_select()?))
    }

    /// A `SELECT` with every clause of §2.
    fn parse_select(&mut self) -> Result<Located<SelectStmt>, SqlError> {
        let at = self.at();
        self.expect_keyword(Keyword::Select, "'SELECT'")?;
        let distinct = self.eat_keyword(Keyword::Distinct)?;
        let items = self.parse_select_list()?;
        self.expect_keyword(Keyword::From, "'FROM'")?;
        let from = self.parse_from_list()?;
        let where_ = if self.eat_keyword(Keyword::Where)? {
            Some(self.parse_condition()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        let mut having = None;
        if self.eat_keyword(Keyword::Group)? {
            self.expect_keyword(Keyword::By, "'BY'")?;
            group_by.push(self.parse_column()?);
            while matches!(self.peek(), TokenKind::Comma) {
                self.advance()?;
                group_by.push(self.parse_column()?);
            }
            if self.eat_keyword(Keyword::Having)? {
                having = Some(self.parse_condition()?);
            }
        }
        let mut order_by = Vec::new();
        if self.eat_keyword(Keyword::Order)? {
            self.expect_keyword(Keyword::By, "'BY'")?;
            order_by.push(self.parse_order_item()?);
            while matches!(self.peek(), TokenKind::Comma) {
                self.advance()?;
                order_by.push(self.parse_order_item()?);
            }
        }
        let limit = if self.eat_keyword(Keyword::Limit)? {
            Some(self.parse_non_negative_int("a LIMIT")?)
        } else {
            None
        };
        Ok(Located::new(
            SelectStmt {
                distinct,
                items,
                from,
                where_,
                group_by,
                having,
                order_by,
                limit,
            },
            at,
        ))
    }

    /// `select_list ::= "*" | select_item { "," select_item }`.
    fn parse_select_list(&mut self) -> Result<SelectItems, SqlError> {
        if matches!(self.peek(), TokenKind::Star) {
            self.advance()?;
            return Ok(SelectItems::Star);
        }
        let mut items = vec![self.parse_select_item()?];
        while matches!(self.peek(), TokenKind::Comma) {
            self.advance()?;
            items.push(self.parse_select_item()?);
        }
        Ok(SelectItems::List(items))
    }

    /// `select_item ::= ( column | aggregate ) [ AS ident ]`.
    fn parse_select_item(&mut self) -> Result<SelectItem, SqlError> {
        let expr = self.parse_expr()?;
        let alias = if self.eat_keyword(Keyword::As)? {
            Some(self.parse_ident("an alias")?)
        } else {
            None
        };
        Ok(SelectItem { expr, alias })
    }

    /// `expr ::= column | aggregate`.
    fn parse_expr(&mut self) -> Result<Expr, SqlError> {
        if let TokenKind::Keyword(kw) = self.peek()
            && kw.is_aggregate()
        {
            Ok(Expr::Aggregate(self.parse_aggregate()?))
        } else {
            Ok(Expr::Column(self.parse_column()?))
        }
    }

    /// `aggregate ::= (COUNT|SUM|AVG|MIN|MAX) "(" (column | "*") ")"`.
    fn parse_aggregate(&mut self) -> Result<Located<AggregateExpr>, SqlError> {
        let at = self.at();
        let func = match self.peek() {
            TokenKind::Keyword(Keyword::Count) => Aggregate::Count,
            TokenKind::Keyword(Keyword::Sum) => Aggregate::Sum,
            TokenKind::Keyword(Keyword::Avg) => Aggregate::Avg,
            TokenKind::Keyword(Keyword::Min) => Aggregate::Min,
            TokenKind::Keyword(Keyword::Max) => Aggregate::Max,
            _ => return Err(self.unexpected("an aggregate function")),
        };
        self.advance()?;
        self.expect(&TokenKind::LParen, "'('")?;
        let arg = if matches!(self.peek(), TokenKind::Star) {
            if func != Aggregate::Count {
                return Err(SqlError::aggregate(
                    format!("'*' is only legal in COUNT(*), not {func}"),
                    self.at(),
                ));
            }
            self.advance()?;
            AggregateArgument::Star
        } else {
            AggregateArgument::Column(self.parse_column()?)
        };
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Located::new(AggregateExpr { func, arg }, at))
    }

    /// `from_list ::= from_item { "," from_item }`.
    fn parse_from_list(&mut self) -> Result<Vec<FromItem>, SqlError> {
        let mut items = vec![self.parse_from_item()?];
        while matches!(self.peek(), TokenKind::Comma) {
            self.advance()?;
            items.push(self.parse_from_item()?);
        }
        Ok(items)
    }

    /// `from_item ::= table_ref { JOIN table_ref ON condition }`, left to
    /// right.
    fn parse_from_item(&mut self) -> Result<FromItem, SqlError> {
        let mut item = FromItem::Table(self.parse_table_ref()?);
        while self.at_keyword(Keyword::Join) {
            self.advance()?;
            let right = self.parse_table_ref()?;
            self.expect_keyword(Keyword::On, "'ON'")?;
            let on = self.parse_condition()?;
            item = FromItem::Join {
                left: Box::new(item),
                right,
                on: Box::new(on),
            };
        }
        Ok(item)
    }

    /// `table_ref ::= ident [ AS ident ] | "(" query ")" [ AS ] ident`.
    fn parse_table_ref(&mut self) -> Result<TableRef, SqlError> {
        if matches!(self.peek(), TokenKind::LParen) {
            self.advance()?;
            let query = self.parse_query()?;
            self.expect(&TokenKind::RParen, "')'")?;
            // The grammar demands an alias for a subquery. `AS` is accepted but
            // not required, so both `(…) AS t` and `(…) t` parse.
            self.eat_keyword(Keyword::As)?;
            let alias = self.parse_ident("an alias for the subquery")?;
            return Ok(TableRef::Subquery {
                query: Box::new(query),
                alias,
            });
        }
        let name = self.parse_ident("a table name")?;
        // Both `AS alias` and a bare `alias` name the table reference.
        let alias = if self.eat_keyword(Keyword::As)? || matches!(self.peek(), TokenKind::Ident(_))
        {
            Some(self.parse_ident("an alias")?)
        } else {
            None
        };
        Ok(TableRef::Base { name, alias })
    }

    /// `order_item ::= column [ ASC | DESC ]`.
    fn parse_order_item(&mut self) -> Result<OrderItem, SqlError> {
        let key = self.parse_column()?;
        let direction = if self.eat_keyword(Keyword::Asc)? {
            Direction::Asc
        } else if self.eat_keyword(Keyword::Desc)? {
            Direction::Desc
        } else {
            Direction::Asc
        };
        Ok(OrderItem { key, direction })
    }

    // =================================================================
    // Conditions
    // =================================================================

    /// `condition ::= or_expr`.
    fn parse_condition(&mut self) -> Result<Condition, SqlError> {
        self.parse_or()
    }

    /// `or_expr ::= and_expr { OR and_expr }`, left-associative.
    fn parse_or(&mut self) -> Result<Condition, SqlError> {
        let mut left = self.parse_and()?;
        while self.at_keyword(Keyword::Or) {
            self.advance()?;
            let right = self.parse_and()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `and_expr ::= not_expr { AND not_expr }`, left-associative.
    fn parse_and(&mut self) -> Result<Condition, SqlError> {
        let mut left = self.parse_not()?;
        while self.at_keyword(Keyword::And) {
            self.advance()?;
            let right = self.parse_not()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `not_expr ::= NOT not_expr | primary_cond`.
    fn parse_not(&mut self) -> Result<Condition, SqlError> {
        if self.at_keyword(Keyword::Not) {
            self.advance()?;
            let inner = self.parse_not()?;
            Ok(Condition::Not(Box::new(inner)))
        } else {
            self.parse_primary_cond()
        }
    }

    /// `primary_cond ::= "(" condition ")" | comparison`.
    fn parse_primary_cond(&mut self) -> Result<Condition, SqlError> {
        if matches!(self.peek(), TokenKind::LParen) {
            self.advance()?;
            let inner = self.parse_condition()?;
            self.expect(&TokenKind::RParen, "')'")?;
            return Ok(inner);
        }
        self.parse_comparison()
    }

    /// `comparison ::= operand cmp operand`.
    fn parse_comparison(&mut self) -> Result<Condition, SqlError> {
        let left = self.parse_operand()?;
        let at = self.at();
        let op = match self.peek() {
            TokenKind::Eq => CompareOp::Eq,
            TokenKind::Ne => CompareOp::Ne,
            TokenKind::Lt => CompareOp::Lt,
            TokenKind::Le => CompareOp::Le,
            TokenKind::Gt => CompareOp::Gt,
            TokenKind::Ge => CompareOp::Ge,
            _ => return Err(self.unexpected("a comparison operator")),
        };
        self.advance()?;
        let right = self.parse_operand()?;
        Ok(Condition::Compare(Located::new(
            Compare { left, op, right },
            at,
        )))
    }

    /// `operand ::= column | aggregate | int | string`.
    fn parse_operand(&mut self) -> Result<Operand, SqlError> {
        match self.peek().clone() {
            TokenKind::Int(value) => {
                self.advance()?;
                Ok(Operand::Literal(Value::Int(value)))
            }
            TokenKind::Str(value) => {
                self.advance()?;
                Ok(Operand::Literal(Value::Varchar(value)))
            }
            TokenKind::Keyword(kw) if kw.is_aggregate() => {
                Ok(Operand::Aggregate(self.parse_aggregate()?))
            }
            TokenKind::Ident(_) => Ok(Operand::Column(self.parse_column()?)),
            _ => Err(self.unexpected("a column, aggregate, number or string")),
        }
    }

    // =================================================================
    // Names and numbers
    // =================================================================

    /// `column ::= ident [ "." ident ]`.
    fn parse_column(&mut self) -> Result<Located<ColumnRef>, SqlError> {
        let first = self.parse_ident("a column name")?;
        let at = first.at();
        if matches!(self.peek(), TokenKind::Dot) {
            self.advance()?;
            let second = self.parse_ident("a column name")?.into_inner();
            Ok(Located::new(
                ColumnRef {
                    qualifier: Some(first.into_inner()),
                    name: second,
                },
                at,
            ))
        } else {
            Ok(Located::new(
                ColumnRef {
                    qualifier: None,
                    name: first.into_inner(),
                },
                at,
            ))
        }
    }

    /// An `ident` token, named `what` in the error if it is not one.
    fn parse_ident(&mut self, what: &str) -> Result<Located<String>, SqlError> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                let at = self.at();
                self.advance()?;
                Ok(Located::new(name, at))
            }
            _ => Err(self.unexpected(what)),
        }
    }

    /// An integer token that must not be negative, as a `u64`.
    fn parse_non_negative_int(&mut self, what: &str) -> Result<u64, SqlError> {
        match *self.peek() {
            TokenKind::Int(value) => {
                let at = self.at();
                if value < 0 {
                    return Err(SqlError::syntax(format!("{what} must not be negative"), at));
                }
                self.advance()?;
                Ok(value as u64)
            }
            _ => Err(self.unexpected(what)),
        }
    }
}

// =====================================================================
// Parse-tree rendering
// =====================================================================

/// Render a whole program as indented syntax trees, separated by blank lines.
pub(crate) fn render_program(statements: &[Statement]) -> String {
    let mut out = String::new();
    for (index, statement) in statements.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&render_statement(statement));
    }
    out
}

/// Parse and render a program's indented syntax tree.
///
/// This is the syntax-only entry point for the `parse` command; it needs no
/// catalog and never exposes the AST.
pub fn render_syntax(input: &str) -> Result<String, SqlError> {
    Ok(render_program(&parse_program(input)?))
}

/// Append `text` as one line at `depth`.
fn push_line(out: &mut String, depth: usize, text: &str) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(text);
    out.push('\n');
}

/// The indented syntax tree of one statement.
fn render_statement(statement: &Statement) -> String {
    let mut out = String::new();
    match statement {
        Statement::Create(create) => {
            push_line(
                &mut out,
                0,
                &format!("CREATE TABLE {}", create.name.as_str()),
            );
            for column in &create.columns {
                push_line(
                    &mut out,
                    1,
                    &format!("{} {}", column.name.as_str(), column.ty),
                );
            }
        }
        Statement::Insert(insert) => {
            push_line(
                &mut out,
                0,
                &format!("INSERT INTO {}", insert.table.as_str()),
            );
            let values: Vec<String> = insert
                .values
                .iter()
                .map(|literal| render_literal(literal))
                .collect();
            push_line(&mut out, 1, &format!("VALUES ({})", values.join(", ")));
        }
        Statement::Query(query) => render_query(&mut out, 0, query),
    }
    out
}

/// One literal, as written.
fn render_literal(literal: &Value) -> String {
    match literal {
        Value::Int(value) => value.to_string(),
        Value::Varchar(value) => format!("'{}'", value.replace('\'', "''")),
    }
}

/// A query with its set operators.
fn render_query(out: &mut String, depth: usize, query: &Query) {
    push_line(out, depth, "QUERY");
    render_block(out, depth + 1, &query.first);
    for (op, block) in &query.rest {
        push_line(out, depth + 1, setop_name(*op));
        render_block(out, depth + 2, block);
    }
}

/// One operand of a set operator.
fn render_block(out: &mut String, depth: usize, block: &SelectBlock) {
    match block {
        SelectBlock::Select(select) => {
            let mut head = String::from("SELECT");
            if select.distinct {
                head.push_str(" DISTINCT");
            }
            push_line(out, depth, &head);
            match &select.items {
                SelectItems::Star => push_line(out, depth + 1, "*"),
                SelectItems::List(items) => {
                    for item in items {
                        let mut text = match &item.expr {
                            Expr::Column(column) => column.to_string(),
                            Expr::Aggregate(aggregate) => render_aggregate(aggregate),
                        };
                        if let Some(alias) = &item.alias {
                            text.push_str(&format!(" AS {}", alias.as_str()));
                        }
                        push_line(out, depth + 1, &text);
                    }
                }
            }
            push_line(out, depth, "FROM");
            for item in &select.from {
                render_from(out, depth + 1, item);
            }
            if let Some(where_) = &select.where_ {
                push_line(out, depth, &format!("WHERE {}", render_condition(where_)));
            }
            if !select.group_by.is_empty() {
                let keys: Vec<String> = select.group_by.iter().map(|c| c.to_string()).collect();
                push_line(out, depth, &format!("GROUP BY {}", keys.join(", ")));
            }
            if let Some(having) = &select.having {
                push_line(out, depth, &format!("HAVING {}", render_condition(having)));
            }
            if !select.order_by.is_empty() {
                let keys: Vec<String> = select
                    .order_by
                    .iter()
                    .map(|item| format!("{} {}", item.key, direction_name(item.direction)))
                    .collect();
                push_line(out, depth, &format!("ORDER BY {}", keys.join(", ")));
            }
            if let Some(limit) = select.limit {
                push_line(out, depth, &format!("LIMIT {limit}"));
            }
        }
        SelectBlock::Sub(query) => {
            push_line(out, depth, "SUBQUERY");
            render_query(out, depth + 1, query);
        }
    }
}

/// One `FROM` item and its join chain.
fn render_from(out: &mut String, depth: usize, item: &FromItem) {
    match item {
        FromItem::Table(table) => render_table_ref(out, depth, table),
        FromItem::Join { left, right, on } => {
            push_line(out, depth, &format!("JOIN ON {}", render_condition(on)));
            render_from(out, depth + 1, left);
            render_table_ref(out, depth + 1, right);
        }
    }
}

/// One table reference.
fn render_table_ref(out: &mut String, depth: usize, table: &TableRef) {
    match table {
        TableRef::Base { name, alias } => {
            let alias = alias
                .as_ref()
                .map_or(String::new(), |a| format!(" AS {}", a.as_str()));
            push_line(out, depth, &format!("TABLE {}{alias}", name.as_str()));
        }
        TableRef::Subquery { query, alias } => {
            push_line(out, depth, &format!("SUBQUERY AS {}", alias.as_str()));
            render_query(out, depth + 1, query);
        }
    }
}

/// An aggregate call, as written.
fn render_aggregate(aggregate: &AggregateExpr) -> String {
    let func = aggregate.func;
    match &aggregate.arg {
        AggregateArgument::Star => format!("{func}(*)"),
        AggregateArgument::Column(column) => format!("{func}({column})"),
    }
}

/// A condition, fully parenthesised.
fn render_condition(condition: &Condition) -> String {
    match condition {
        Condition::Compare(compare) => format!(
            "{} {} {}",
            render_operand(&compare.left),
            compare_name(compare.op),
            render_operand(&compare.right)
        ),
        Condition::And(a, b) => {
            format!("({} and {})", render_condition(a), render_condition(b))
        }
        Condition::Or(a, b) => {
            format!("({} or {})", render_condition(a), render_condition(b))
        }
        Condition::Not(inner) => format!("(not {})", render_condition(inner)),
    }
}

/// One operand of a condition.
fn render_operand(operand: &Operand) -> String {
    match operand {
        Operand::Column(column) => column.to_string(),
        Operand::Aggregate(aggregate) => render_aggregate(aggregate),
        Operand::Literal(Value::Int(value)) => value.to_string(),
        Operand::Literal(Value::Varchar(value)) => format!("'{}'", value.replace('\'', "''")),
    }
}

/// The canonical spelling of a comparison operator.
fn compare_name(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::Ne => "<>",
        CompareOp::Lt => "<",
        CompareOp::Le => "<=",
        CompareOp::Gt => ">",
        CompareOp::Ge => ">=",
    }
}

/// The upper-case spelling of a set operator.
fn setop_name(op: SetOp) -> &'static str {
    match op {
        SetOp::Union => "UNION",
        SetOp::UnionAll => "UNION ALL",
        SetOp::Intersect => "INTERSECT",
        SetOp::Except => "EXCEPT",
    }
}

/// The plan's spelling of a sort direction.
fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Asc => "asc",
        Direction::Desc => "desc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain filter parses into the AST §5.1 describes.
    #[test]
    fn parses_a_plain_select() {
        let statements = parse_program("SELECT name, age FROM Emp WHERE age > 30;").unwrap();
        let Statement::Query(query) = &statements[0] else {
            panic!("expected a query");
        };
        let SelectBlock::Select(select) = &query.first else {
            panic!("expected a select");
        };
        assert_eq!(select.from.len(), 1);
        assert!(select.where_.is_some());
        assert!(matches!(select.items, SelectItems::List(ref l) if l.len() == 2));
    }

    /// Set operators are left-associative and share one precedence.
    #[test]
    fn set_operators_fold_left() {
        let statements =
            parse_program("SELECT a FROM R UNION SELECT a FROM S EXCEPT SELECT a FROM T").unwrap();
        let Statement::Query(query) = &statements[0] else {
            panic!("expected a query");
        };
        assert_eq!(
            query.rest.iter().map(|(op, _)| *op).collect::<Vec<_>>(),
            vec![SetOp::Union, SetOp::Except]
        );
    }

    /// `NOT` binds tighter than `AND`, which binds tighter than `OR`.
    #[test]
    fn condition_precedence_holds() {
        let statements = parse_program("SELECT a FROM R WHERE a = 1 OR b = 2 AND c = 3").unwrap();
        let Statement::Query(query) = &statements[0] else {
            panic!("expected a query");
        };
        let SelectBlock::Select(select) = &query.first else {
            panic!("expected a select");
        };
        let Some(Condition::Or(_, right)) = select.where_.as_ref() else {
            panic!("expected OR at the root");
        };
        assert!(matches!(right.as_ref(), Condition::And(_, _)));
    }

    /// `<>` and `!=` both parse as not-equal.
    #[test]
    fn both_not_equal_spellings_parse() {
        for source in [
            "SELECT a FROM R WHERE a <> 1",
            "SELECT a FROM R WHERE a != 1",
        ] {
            let statements = parse_program(source).unwrap();
            let Statement::Query(query) = &statements[0] else {
                panic!("expected a query");
            };
            let SelectBlock::Select(select) = &query.first else {
                panic!("expected a select");
            };
            let Some(Condition::Compare(compare)) = select.where_.as_ref() else {
                panic!("expected a comparison");
            };
            assert_eq!(compare.op, CompareOp::Ne);
        }
    }

    /// A statement without a terminating semicolon still parses.
    #[test]
    fn final_semicolon_is_optional() {
        assert!(parse_program("SELECT a FROM R").is_ok());
    }
}
