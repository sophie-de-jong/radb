//! Parser, abstract syntax tree and parse-tree rendering.
//!
//! A hand-written recursive-descent parser that owns a [`Tokenizer`] and pulls
//! tokens from it lazily, one method per nonterminal of GRAMMAR.md §1.2. The
//! grammar's precedence levels are the method levels: `SetExpr` → `JoinExpr` →
//! `Unary` → `Atom` and `OrExpr` → `AndExpr` → `NotExpr` → `PrimaryCond`, each
//! looping on its operators to fold them into the already-parsed left side.
//!
//! It also parses §4.1 relation definitions (`NAME ( attrs ) = { ... }`) — see
//! [`Parser::parse_relation_def`] and [`parse_relation`].
//!
//! Per GRAMMAR.md §"Keywords as attribute names", a keyword token is accepted
//! wherever the grammar expects an attribute name (see
//! [`Parser::parse_attr_name`]).

use std::error::Error as StdError;
use std::fmt;

use crate::engine::{RowError, SemanticError};
use crate::tokenizer::{Keyword, LexError, Position, Token, TokenKind, Tokenizer};
use crate::Relation;
use crate::Value;

// =====================================================================
// Parse errors
// =====================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// A lexical error, reported unchanged.
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
    /// Blank or comment-only input: no relation definition was given.
    MissingHeader,
    /// Two attributes in one header share a name; `at` is the header's `(`.
    DuplicateAttribute {
        name: String,
        at: Position,
    },
    /// The header is well-formed but does not make a relation.
    InvalidRelation {
        detail: String,
        at: Position,
    },
    /// A header used a qualified attribute name, `Q(D.Name)`; see GRAMMAR.md
    /// §"Qualified names".
    QualifiedAttributeName {
        name: String,
        at: Position,
    },
    /// A tuple has more or fewer values than the relation has attributes;
    /// `at` is the tuple's first value.
    ArityMismatch {
        at: Position,
        expected: usize,
        found: usize,
    },
    /// A column holds values of two types, e.g. `R(x) = {1, 'a'}`; `expected`
    /// and `found` are `"int"` or `"str"`, and `at` is the offending tuple.
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
    /// Renders the error as a one-line message with its position.
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
            ParseError::InvalidRelation { detail, at } => {
                write!(f, "cannot build the relation declared at {at}: {detail}")
            }
            ParseError::QualifiedAttributeName { name, at } => {
                write!(f, "'{name}' at {at} cannot be an attribute name in a relation header: attribute names are identifiers (§4.1), and a qualified one could never be referred to")
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
    /// A lexical error is a parse error whose position and wording stand.
    fn from(e: LexError) -> Self {
        ParseError::Lex(e)
    }
}

// =====================================================================
// AST
// =====================================================================

/// A parsed query: one node per operator, with its condition or children.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// A reference to a loaded relation.
    Variable(String),
    /// `select[cond](input)`.
    Select {
        predicate: Predicate,
        input: Box<Query>,
    },
    /// `project[attrs](input)`.
    Project {
        attrs: Vec<String>,
        input: Box<Query>,
    },
    /// `rename[name](input)`.
    Rename {
        new_name: String,
        input: Box<Query>,
    },
    /// `left join[cond] right`.
    Join {
        condition: Predicate,
        left: Box<Query>,
        right: Box<Query>,
    },
    /// `left union right`.
    Union {
        left: Box<Query>,
        right: Box<Query>,
    },
    /// `left intersect right`.
    Intersect {
        left: Box<Query>,
        right: Box<Query>,
    },
    /// `left minus right`.
    Minus {
        left: Box<Query>,
        right: Box<Query>,
    },
    /// `left times right`.
    Times {
        left: Box<Query>,
        right: Box<Query>,
    },
}

impl fmt::Display for Query {
    /// Renders the query as an indented parse tree, one node per line.
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

/// A condition: one comparison, combined with `and`, `or` and `not`.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `left op right`.
    Compare {
        left: Operand,
        op: CompareOp,
        right: Operand,
    },
    /// Both operands must hold.
    And(Box<Predicate>, Box<Predicate>),
    /// Either operand must hold.
    Or(Box<Predicate>, Box<Predicate>),
    /// The operand must not hold.
    Not(Box<Predicate>),
}

impl fmt::Display for Predicate {
    /// Renders the condition in the `Op(...)` form used in parse trees.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::Compare { left, op, right } => write!(f, "{op:?}({left}, {right})"),
            Predicate::And(a, b) => write!(f, "And({a}, {b})"),
            Predicate::Or(a, b) => write!(f, "Or({a}, {b})"),
            Predicate::Not(a) => write!(f, "Not({a})"),
        }
    }
}

/// One side of a comparison: an attribute, an integer or a string.
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// An attribute name, possibly qualified as `Rel.Attr`.
    Attr(String),
    /// An integer literal.
    Num(i64),
    /// A string literal.
    Str(String),
}

impl fmt::Display for Operand {
    /// Renders the operand with its kind, e.g. `Attr(Age)` or `Str('x')`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Attr(name) => write!(f, "Attr({name})"),
            Operand::Num(i) => write!(f, "Num({i})"),
            Operand::Str(s) => write!(f, "Str('{}')", s.replace('\'', "''")),
        }
    }
}

/// The six comparison operators of GRAMMAR.md §1.2.
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

/// A recursive-descent parser over a [`Tokenizer`], with one method per
/// nonterminal of GRAMMAR.md §1.2 and a single token of lookahead.
pub struct Parser<'a> {
    tokenizer: Tokenizer<'a>,
    current: Token,
}

impl<'a> Parser<'a> {
    /// A parser for `input`, with the first token already read.
    ///
    /// Errors: a [`LexError`] in the first token.
    pub fn new(input: &'a str) -> Result<Self, ParseError> {
        let mut tokenizer = Tokenizer::new(input);
        let current = tokenizer.next_token()?;
        Ok(Parser { tokenizer, current })
    }

    /// The position of the current token in the input.
    fn at(&self) -> Position {
        Position::new(self.tokenizer.input(), self.current.at)
    }

    /// The kind of the current token.
    fn peek(&self) -> &TokenKind {
        &self.current.kind
    }

    /// Consume the current token and read the next one.
    ///
    /// Errors: a [`LexError`] in the next token.
    fn advance(&mut self) -> Result<(), ParseError> {
        self.current = self.tokenizer.next_token()?;
        Ok(())
    }

    /// Consume the current token if it is `kind`, else report that `what` was
    /// expected there.
    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<(), ParseError> {
        if self.peek() == kind {
            self.advance()
        } else {
            Err(self.unexpected(what))
        }
    }

    /// A parse error for the current token naming `expected` as what belonged
    /// there; end of input gives [`ParseError::UnexpectedEof`].
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
    ///
    /// Errors: any [`ParseError`] in the definition, plus
    /// [`ParseError::MissingHeader`] for blank or comment-only input.
    pub fn parse_relation_def(&mut self) -> Result<(String, Relation), ParseError> {
        if matches!(self.peek(), TokenKind::Eof) {
            return Err(ParseError::MissingHeader);
        }

        let name = self.parse_name()?; // NAME
        self.expect(&TokenKind::LParen, "'(' after relation name")?;
        // Kept as the position to blame if the header turns out to be invalid.
        let header_at = self.at();
        let schema = self.parse_attr_list()?; // AttrList
        self.expect(&TokenKind::RParen, "')' after attribute list")?;
        self.expect(&TokenKind::Eq, "'='")?;
        self.expect(&TokenKind::LBrace, "'{'")?;
        let mut relation = Relation::new(schema).map_err(|error| match error {
            SemanticError::DuplicateColumn { name } => ParseError::DuplicateAttribute {
                name,
                at: header_at,
            },
            other => ParseError::InvalidRelation {
                detail: other.to_string(),
                at: header_at,
            },
        })?;
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

    /// AttrList ::= BareAttrName ( "," BareAttrName )+
    ///
    /// A repeated name is not rejected here: it is left to [`Relation::new`].
    fn parse_attr_list(&mut self) -> Result<Vec<String>, ParseError> {
        let mut attrs = vec![self.parse_bare_attr_name()?];
        while matches!(self.peek(), TokenKind::Comma) {
            self.advance()?;
            attrs.push(self.parse_bare_attr_name()?);
        }
        Ok(attrs)
    }

    /// BareAttrName ::= IDENT | KEYWORD
    ///
    /// A qualified name is refused here: GRAMMAR.md §"Qualified names" gives
    /// the reason, and [`ParseError::QualifiedAttributeName`] the report.
    fn parse_bare_attr_name(&mut self) -> Result<String, ParseError> {
        let at = self.at();
        if let TokenKind::QualIdent(name) = self.peek() {
            return Err(ParseError::QualifiedAttributeName {
                name: name.clone(),
                at,
            });
        }
        self.parse_attr_name()
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
    /// Tuples are added to `relation` as they are read, so a tuple of the
    /// wrong arity or with a mixed-type column is reported here. The closing
    /// `}` is left for [`Parser::parse_relation_def`].
    fn parse_tuple_list(&mut self, relation: &mut Relation) -> Result<(), ParseError> {
        while !matches!(self.peek(), TokenKind::RBrace) {
            if matches!(self.peek(), TokenKind::Eof) {
                return Err(ParseError::MissingClosingBrace { at: self.at() });
            }
            let at = self.at();
            let values = self.parse_tuple()?;
            match relation.insert(values) {
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
    ///
    /// Errors: any [`ParseError`], including trailing input after the query.
    pub fn parse_query(&mut self) -> Result<Query, ParseError> {
        let query = self.parse_set_expr()?;
        if !matches!(self.peek(), TokenKind::Eof) {
            return Err(self.unexpected("end of input"));
        }
        Ok(query)
    }

    /// SetExpr ::= JoinExpr ( SetOp JoinExpr )* — one precedence level,
    /// left-associative (GRAMMAR.md §2.1).
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

    /// JoinExpr ::= Unary ( ( "times" Unary )
    ///                     | ( "join" "[" Cond "]" Unary ) )*
    /// — tighter than the set operators, left-associative.
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

    /// OrExpr ::= AndExpr ( "or" AndExpr )* — loosest condition level,
    /// left-associative.
    fn parse_or(&mut self) -> Result<Predicate, ParseError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), TokenKind::Keyword(Keyword::Or)) {
            self.advance()?;
            let right = self.parse_and()?;
            left = Predicate::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// AndExpr ::= NotExpr ( "and" NotExpr )* — binds tighter than `or`,
    /// left-associative.
    fn parse_and(&mut self) -> Result<Predicate, ParseError> {
        let mut left = self.parse_not()?;
        while matches!(self.peek(), TokenKind::Keyword(Keyword::And)) {
            self.advance()?;
            let right = self.parse_not()?;
            left = Predicate::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// NotExpr ::= "not" NotExpr | PrimaryCond — prefix, binds tighter than
    /// `and`.
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

    /// Comparison ::= Operand CmpOp Operand — non-associative, so `a<b<c` is
    /// a syntax error.
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
    ///
    /// A keyword token is accepted as an attribute name here, which is what
    /// makes `select[union=3](R)` run against a real column (GRAMMAR.md
    /// §"Keywords as attribute names").
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
///
/// Errors: any [`ParseError`], including a [`LexError`] in the first token.
pub fn parse_query(input: &str) -> Result<Query, ParseError> {
    let mut parser = Parser::new(input)?;
    parser.parse_query()
}

/// Parse a whole §4.1 relation definition into its header name and relation.
///
/// Errors: any [`ParseError`] in the definition.
pub fn parse_relation(input: &str) -> Result<(String, Relation), ParseError> {
    let mut parser = Parser::new(input)?;
    parser.parse_relation_def()
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn dotted_header_attribute_is_rejected() {
        // §4.1: "Attribute names are identifiers." A dotted one is unusable —
        // a times or join would prefix the relation name to give `Q.D.Name`,
        // which the tokenizer never lexes back, so the column could be printed
        // but never selected on, projected or joined.
        let err = parse_relation("Q(D.Name, Age) = {\n'Ann', 30\n}").unwrap_err();
        match err {
            ParseError::QualifiedAttributeName { ref name, .. } => {
                assert_eq!(name, "D.Name", "the error should name the offending attribute");
            }
            other => panic!("expected QualifiedAttributeName, got {other:#?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("identifier") && msg.contains("D.Name"),
            "error message should explain the rule and name the attribute, got: {msg}"
        );

        // A bare identifier header is still fine, including a keyword as a name
        // (row #8) and a column that happens to match its relation name.
        assert!(parse_relation("Q(DName, Age) = {\n'Ann', 30\n}").is_ok());
        assert!(parse_relation("R(union, x) = {\n3, 1\n}").is_ok());
        assert!(parse_relation("K(K) = {\n1\n}").is_ok());
    }

    #[test]
    fn repeated_header_attribute_is_rejected_with_a_position() {
        // The repetition is `Relation::new`'s to notice, not the parser's: that is
        // the single place a hand-written header list is checked, so a header that
        // repeats a name is refused the same way whether it came from a file or
        // from a caller. The parser's job here is only to supply a position.
        let err = parse_relation("R(a, b, a) = {\n1, 2, 3\n}").unwrap_err();
        match err {
            ParseError::DuplicateAttribute { ref name, at } => {
                assert_eq!(name, "a", "the error should name the repeated attribute");
                assert_eq!(at.line, 1, "the header is on the first line");
                assert!(
                    at.col > 0,
                    "the position should point into the header, got {at}"
                );
            }
            other => panic!("expected DuplicateAttribute, got {other:#?}"),
        }

        // The message has to stand on its own, since it is what a user sees.
        let msg = parse_relation("R(a, a) = {\n1, 2\n}").unwrap_err().to_string();
        assert!(
            msg.contains("duplicate attribute 'a'") && msg.contains("line 1"),
            "error message should name the attribute and the line, got: {msg}"
        );

        // Distinct names, including one equal to the relation name (row #26 in
        // 3_semantics.rs) and a keyword used as a name (row #8), are all fine.
        assert!(parse_relation("R(a, b) = {\n1, 2\n}").is_ok());
    }
}