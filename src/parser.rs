//! Parser, abstract syntax tree and parse-tree rendering.
//!
//! The parser is a hand-written recursive-descent parser that owns a
//! [`Tokenizer`] and pulls tokens from it lazily. It implements the grammar
//! documented in GRAMMAR.md; in particular, precedence is stratified into
//! levels (`SetExpr` → `JoinExpr` → `Unary`, and `OrExpr` → `AndExpr` →
//! `NotExpr` → `PrimaryCond`), which is what makes the grammar unambiguous.
//!
//! It also parses §4.1 relation definitions (`NAME ( attrs ) = { ... }`)
//! — see [`Parser::parse_relation_def`] and [`parse_relation`].
//!
//! Per GRAMMAR.md §"Keywords as attribute names", the parser — not the
//! lexer — accepts a keyword token wherever the grammar expects an attribute
//! name (see [`Parser::parse_attr_name`]).

use std::error::Error as StdError;
use std::fmt;

use crate::engine::RowError;
use crate::tokenizer::{Keyword, LexError, Position, Token, TokenKind, Tokenizer};
use crate::Relation;
use crate::Value;

// =====================================================================
// Parse errors
// =====================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    Lex(LexError),
    /// Ran out of input while a construct was still open (e.g. a missing `)`).
    UnexpectedEof {
        at: Position,
        expected: String,
    },
    /// Saw a token where a different kind of token was required.
    UnexpectedToken {
        found: String,
        expected: String,
        at: Position,
    },
    /// `project[](R)` — the attribute list must have at least one name.
    EmptyProjectionList {
        at: Position,
    },
    /// A blank/comment-only input contains no relation definition: no
    /// `NAME` ever appears.
    MissingHeader,
    /// Two attributes in one header have the same name; `at` is the second
    /// occurrence.
    DuplicateAttribute {
        name: String,
        at: Position,
    },
    /// A tuple had more or fewer values than the relation has attributes.
    /// `Relation::push` enforces §1.2's arity rule after the parser reads
    /// the tuple, so this error points at the tuple's starting position.
    ArityMismatch {
        at: Position,
        expected: usize,
        found: usize,
    },
    /// A column holds values of more than one type (e.g. `R(x) = {1, 'a'}`).
    /// A column's type is its values' type, so mixing is a load-time error:
    /// `Relation::push` refuses the tuple and this error names the column,
    /// the two types, and the position of the tuple that carried the
    /// offending value (its starting position). Both types are "int" or
    /// "str".
    ColumnTypeMismatch {
        at: Position,
        name: String,
        expected: &'static str,
        found: &'static str,
    },
    /// A relation definition never reached its closing `}`.
    MissingClosingBrace {
        at: Position,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Lex(e) => write!(f, "{e}"),
            ParseError::UnexpectedEof { at, expected } => {
                write!(f, "unexpected end of input at {at}, expected {expected}")
            }
            ParseError::UnexpectedToken {
                found,
                expected,
                at,
            } => write!(f, "unexpected {found} at {at}, expected {expected}"),
            ParseError::EmptyProjectionList { at } => {
                write!(f, "empty attribute list in project[] at {at}")
            }
            ParseError::MissingHeader => {
                write!(f, "relation definition has no header (NAME (attrs) = {{)")
            }
            ParseError::DuplicateAttribute { name, at } => {
                write!(f, "duplicate attribute '{name}' at {at} in relation header")
            }
            ParseError::ArityMismatch {
                at,
                expected,
                found,
            } => {
                write!(f, "tuple at {at} has {found} values but the relation has {expected} attributes")
            }
            ParseError::ColumnTypeMismatch {
                at,
                name,
                expected,
                found,
            } => {
                write!(f, "column '{name}' at {at} has type {found}, but previous values in the column are {expected}")
            }
            ParseError::MissingClosingBrace { at } => {
                write!(f, "relation definition never reached '}}' (at {at})")
            }
        }
    }
}
impl StdError for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::Lex(e)
    }
}

// =====================================================================
// AST
// =====================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Variable(String),
    Select {
        predicate: Predicate,
        input: Box<Query>,
    },
    Project {
        attrs: Vec<String>,
        input: Box<Query>,
    },
    Rename {
        new_name: String,
        input: Box<Query>,
    },
    Join {
        condition: Predicate,
        left: Box<Query>,
        right: Box<Query>,
    },
    Union {
        left: Box<Query>,
        right: Box<Query>,
    },
    Intersect {
        left: Box<Query>,
        right: Box<Query>,
    },
    Minus {
        left: Box<Query>,
        right: Box<Query>,
    },
    Times {
        left: Box<Query>,
        right: Box<Query>,
    },
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let children = match self {
            Query::Variable(name) => {
                write!(f, "Relation({name})")?;
                vec![]
            }
            Query::Select { predicate, input } => {
                write!(f, "Select(cond={predicate})")?;
                vec![input]
            }
            Query::Project { attrs, input } => {
                write!(f, "Project(attrs=[{}])", attrs.join(", "))?;
                vec![input]
            }
            Query::Rename { new_name, input } => {
                write!(f, "Rename(name={new_name})")?;
                vec![input]
            }
            Query::Join {
                condition,
                left,
                right,
            } => {
                write!(f, "Join(cond={condition})")?;
                vec![left, right]
            }
            Query::Union { left, right } => {
                f.write_str("Union")?;
                vec![left, right]
            }
            Query::Intersect { left, right } => {
                f.write_str("Intersect")?;
                vec![left, right]
            }
            Query::Minus { left, right } => {
                f.write_str("Minus")?;
                vec![left, right]
            }
            Query::Times { left, right } => {
                f.write_str("Times")?;
                vec![left, right]
            }
        };

        let last = children.len().saturating_sub(1);
        for (i, child) in children.iter().enumerate() {
            let connector = if i == last {
                "└── "
            } else {
                "├── "
            };
            let pad = if i == last { "    " } else { "│   " };
            let subtree = child.to_string();
            for (j, line) in subtree.lines().enumerate() {
                write!(f, "\n{}{}", if j == 0 { connector } else { pad }, line)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Compare {
        left: Operand,
        op: CompareOp,
        right: Operand,
    },
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::Compare { left, op, right } => write!(f, "{op:?}({left}, {right})"),
            Predicate::And(a, b) => write!(f, "And({a}, {b})"),
            Predicate::Or(a, b) => write!(f, "Or({a}, {b})"),
            Predicate::Not(a) => write!(f, "Not({a})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Attr(String),
    Num(i64),
    Str(String),
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Attr(name) => write!(f, "Attr({name})"),
            Operand::Num(i) => write!(f, "Num({i})"),
            Operand::Str(s) => write!(f, "Str('{}')", s.replace('\'', "''")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

// =====================================================================
// Parser
// =====================================================================

pub struct Parser<'a> {
    tokenizer: Tokenizer<'a>,
    current: Token,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Result<Self, ParseError> {
        let mut tokenizer = Tokenizer::new(input);
        let current = tokenizer.next_token()?;
        Ok(Parser { tokenizer, current })
    }

    fn at(&self) -> Position {
        Position::new(self.tokenizer.input(), self.current.at)
    }

    fn peek(&self) -> &TokenKind {
        &self.current.kind
    }

    fn advance(&mut self) -> Result<(), ParseError> {
        self.current = self.tokenizer.next_token()?;
        Ok(())
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<(), ParseError> {
        if self.peek() == kind {
            self.advance()
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        match self.peek() {
            TokenKind::Eof => ParseError::UnexpectedEof {
                at: self.at(),
                expected: expected.to_string(),
            },
            _ => ParseError::UnexpectedToken {
                found: self.peek().describe(),
                expected: expected.to_string(),
                at: self.at(),
            },
        }
    }

    /// RelationDef ::= NAME "(" AttrList ")" "=" "{" TupleList "}"
    pub fn parse_relation_def(&mut self) -> Result<(String, Relation), ParseError> {
        // Blank/comment-only input never produces a token at all.
        if matches!(self.peek(), TokenKind::Eof) {
            return Err(ParseError::MissingHeader);
        }

        let name = self.parse_name()?; // NAME
        self.expect(&TokenKind::LParen, "'(' after relation name")?;
        let schema = self.parse_attr_list()?; // AttrList
        self.expect(&TokenKind::RParen, "')' after attribute list")?;
        self.expect(&TokenKind::Eq, "'='")?;
        self.expect(&TokenKind::LBrace, "'{'")?;
        let mut relation = Relation::new(schema);
        self.parse_tuple_list(&mut relation)?; // TupleList
        self.expect(&TokenKind::RBrace, "'}' after the tuples")?;

        if !matches!(self.peek(), TokenKind::Eof) {
            return Err(self.unexpected("end of input after the relation definition"));
        }
        Ok((name, relation))
    }

    /// NAME ::= IDENT
    fn parse_name(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            TokenKind::Ident(name) => {
                let name = name.clone();
                self.advance()?;
                Ok(name)
            }
            _ => Err(self.unexpected("a relation name")),
        }
    }

    /// AttrList ::= AttrName ( "," AttrName )+
    fn parse_attr_list(&mut self) -> Result<Vec<String>, ParseError> {
        let mut attrs = vec![self.parse_attr_name()?];
        while matches!(self.peek(), TokenKind::Comma) {
            self.advance()?;
            let at = self.at();
            let name = self.parse_attr_name()?;
            if attrs.contains(&name) {
                return Err(ParseError::DuplicateAttribute { name, at });
            }
            attrs.push(name);
        }
        Ok(attrs)
    }

    /// Tuple ::= Value ( "," Value )*
    fn parse_tuple(&mut self) -> Result<Vec<Value>, ParseError> {
        let mut values: Vec<Value> = Vec::new();

        loop {
            let value = match self.peek() {
                TokenKind::Int(i) => Value::Int(*i),
                TokenKind::Str(s) => Value::Str(s.clone()),
                TokenKind::Ident(s) | TokenKind::QualIdent(s) => Value::Str(s.clone()),
                TokenKind::Keyword(kw) => Value::Str(kw.to_string()),
                _ => return Err(self.unexpected("a value")),
            };
            values.push(value);
            self.advance()?;

            match self.peek() {
                TokenKind::Comma => self.advance()?,
                _ => return Ok(values),
            }
        }
    }

    /// TupleList ::= Tuple ( Tuple )*
    ///
    /// Stops at the closing `}` (left unconsumed for
    /// [`Parser::parse_relation_def`]) and reports a missing `}` at end of
    /// input.
    fn parse_tuple_list(&mut self, relation: &mut Relation) -> Result<(), ParseError> {
        while !matches!(self.peek(), TokenKind::RBrace) {
            if matches!(self.peek(), TokenKind::Eof) {
                return Err(ParseError::MissingClosingBrace { at: self.at() });
            }
            let at = self.at();
            let values = self.parse_tuple()?;
            match relation.push(values) {
                Ok(_) => (),
                Err(RowError::Arity { expected, found }) => {
                    return Err(ParseError::ArityMismatch {
                        at,
                        expected,
                        found,
                    })
                }
                Err(RowError::Type {
                    name,
                    expected,
                    found,
                    ..
                }) => {
                    return Err(ParseError::ColumnTypeMismatch {
                        at,
                        name,
                        expected,
                        found,
                    })
                }
            }
        }
        Ok(())
    }

    /// Parse a whole query: a [`Query`] followed by end of input.
    pub fn parse_query(&mut self) -> Result<Query, ParseError> {
        let query = self.parse_set_expr()?;
        if !matches!(self.peek(), TokenKind::Eof) {
            return Err(self.unexpected("end of input"));
        }
        Ok(query)
    }

    /// SetExpr ::= JoinExpr ( SetOp JoinExpr )*
    fn parse_set_expr(&mut self) -> Result<Query, ParseError> {
        let mut left = self.parse_join_expr()?;
        loop {
            match self.peek() {
                TokenKind::Keyword(Keyword::Union) => {
                    self.advance()?;
                    let right = self.parse_join_expr()?;
                    left = Query::Union {
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                TokenKind::Keyword(Keyword::Intersect) => {
                    self.advance()?;
                    let right = self.parse_join_expr()?;
                    left = Query::Intersect {
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                TokenKind::Keyword(Keyword::Minus) => {
                    self.advance()?;
                    let right = self.parse_join_expr()?;
                    left = Query::Minus {
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// JoinExpr ::= Unary ( ( "times" Unary ) | ( "join" "[" Cond "]" Unary ) )*
    fn parse_join_expr(&mut self) -> Result<Query, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                TokenKind::Keyword(Keyword::Times) => {
                    self.advance()?;
                    let right = self.parse_unary()?;
                    left = Query::Times {
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                TokenKind::Keyword(Keyword::Join) => {
                    self.advance()?;
                    self.expect(&TokenKind::LBracket, "'['")?;
                    let condition = self.parse_cond()?;
                    self.expect(&TokenKind::RBracket, "']'")?;
                    let right = self.parse_unary()?;
                    left = Query::Join {
                        condition,
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// Unary ::= "select"  "[" Cond     "]" "(" Expr ")"
    ///         | "project" "[" ProjList "]" "(" Expr ")"
    ///         | "rename"  "[" NewName  "]" "(" Expr ")"
    ///         | Atom
    fn parse_unary(&mut self) -> Result<Query, ParseError> {
        match self.peek() {
            TokenKind::Keyword(Keyword::Select) => {
                self.advance()?;
                self.expect(&TokenKind::LBracket, "'['")?;
                let predicate = self.parse_cond()?;
                self.expect(&TokenKind::RBracket, "']'")?;
                self.expect(&TokenKind::LParen, "'('")?;
                let input = self.parse_set_expr()?;
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(Query::Select {
                    predicate,
                    input: Box::new(input),
                })
            }
            TokenKind::Keyword(Keyword::Project) => {
                self.advance()?;
                self.expect(&TokenKind::LBracket, "'['")?;
                if matches!(self.peek(), TokenKind::RBracket) {
                    return Err(ParseError::EmptyProjectionList { at: self.at() });
                }
                let mut attrs = vec![self.parse_attr_name()?];
                while matches!(self.peek(), TokenKind::Comma) {
                    self.advance()?;
                    attrs.push(self.parse_attr_name()?);
                }
                self.expect(&TokenKind::RBracket, "']'")?;
                self.expect(&TokenKind::LParen, "'('")?;
                let input = self.parse_set_expr()?;
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(Query::Project {
                    attrs,
                    input: Box::new(input),
                })
            }
            TokenKind::Keyword(Keyword::Rename) => {
                self.advance()?;
                self.expect(&TokenKind::LBracket, "'['")?;
                let new_name = self.parse_attr_name()?;
                self.expect(&TokenKind::RBracket, "']'")?;
                self.expect(&TokenKind::LParen, "'('")?;
                let input = self.parse_set_expr()?;
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(Query::Rename {
                    new_name,
                    input: Box::new(input),
                })
            }
            TokenKind::LParen => {
                self.advance()?;
                let inner = self.parse_set_expr()?;
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(inner)
            }
            TokenKind::Ident(ident) => {
                let name = ident.clone();
                self.advance()?;
                Ok(Query::Variable(name))
            }
            _ => Err(self.unexpected("relation name or '('")),
        }
    }

    /// Cond ::= OrExpr
    fn parse_cond(&mut self) -> Result<Predicate, ParseError> {
        self.parse_or()
    }

    /// OrExpr ::= AndExpr ( "or" AndExpr )*
    fn parse_or(&mut self) -> Result<Predicate, ParseError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), TokenKind::Keyword(Keyword::Or)) {
            self.advance()?;
            let right = self.parse_and()?;
            left = Predicate::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// AndExpr ::= NotExpr ( "and" NotExpr )*
    fn parse_and(&mut self) -> Result<Predicate, ParseError> {
        let mut left = self.parse_not()?;
        while matches!(self.peek(), TokenKind::Keyword(Keyword::And)) {
            self.advance()?;
            let right = self.parse_not()?;
            left = Predicate::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// NotExpr ::= "not" NotExpr | PrimaryCond
    fn parse_not(&mut self) -> Result<Predicate, ParseError> {
        if matches!(self.peek(), TokenKind::Keyword(Keyword::Not)) {
            self.advance()?;
            let inner = self.parse_not()?;
            Ok(Predicate::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }

    /// PrimaryCond ::= "(" Cond ")" | Comparison
    fn parse_primary(&mut self) -> Result<Predicate, ParseError> {
        if matches!(self.peek(), TokenKind::LParen) {
            self.advance()?;
            let inner = self.parse_cond()?;
            self.expect(&TokenKind::RParen, "')'")?;
            Ok(inner)
        } else {
            self.parse_comparison()
        }
    }

    /// Comparison ::= Operand CmpOp Operand
    fn parse_comparison(&mut self) -> Result<Predicate, ParseError> {
        let left = self.parse_operand()?;
        let op = match self.peek() {
            TokenKind::Eq => CompareOp::Eq,
            TokenKind::Ne => CompareOp::Ne,
            TokenKind::Lt => CompareOp::Lt,
            TokenKind::Le => CompareOp::Le,
            TokenKind::Gt => CompareOp::Gt,
            TokenKind::Ge => CompareOp::Ge,
            _ => return Err(self.unexpected("comparison operator")),
        };
        self.advance()?;
        let right = self.parse_operand()?;
        Ok(Predicate::Compare { left, op, right })
    }

    /// Operand ::= INT | STRING | AttrName
    fn parse_operand(&mut self) -> Result<Operand, ParseError> {
        match self.peek() {
            TokenKind::Int(i) => {
                let v = *i;
                self.advance()?;
                Ok(Operand::Num(v))
            }
            TokenKind::Str(s) => {
                let v = s.clone();
                self.advance()?;
                Ok(Operand::Str(v))
            }
            TokenKind::Ident(s) | TokenKind::QualIdent(s) => {
                let v = s.clone();
                self.advance()?;
                Ok(Operand::Attr(v))
            }
            TokenKind::Keyword(kw) => {
                let v = kw.to_string();
                self.advance()?;
                Ok(Operand::Attr(v))
            }
            _ => Err(self.unexpected("number, string, or attribute name")),
        }
    }

    /// AttrName ::= IDENT | KEYWORD
    fn parse_attr_name(&mut self) -> Result<String, ParseError> {
        let name = match self.peek() {
            TokenKind::Ident(s) | TokenKind::QualIdent(s) => s.clone(),
            TokenKind::Keyword(kw) => kw.to_string(),
            _ => return Err(self.unexpected("attribute name")),
        };
        self.advance()?;
        Ok(name)
    }
}

/// Parse a whole query string into its abstract syntax tree.
pub fn parse_query(input: &str) -> Result<Query, ParseError> {
    let mut parser = Parser::new(input)?;
    parser.parse_query()
}

/// Parse a whole relation definition into its header name and relation.
pub fn parse_relation(input: &str) -> Result<(String, Relation), ParseError> {
    let mut parser = Parser::new(input)?;
    parser.parse_relation_def()
}
