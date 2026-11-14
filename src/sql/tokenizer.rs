//! The SQL scanner (§1 of the M3 specification).
//!
//! A hand-written character scanner, in the same style as Bonus Project 1's:
//! one character at a time, maximal munch (on `>` it looks one character ahead
//! before choosing `>` or `>=`), no regular expressions and no parser
//! generator. It produces the tokens of the SQL subset, each carrying the line
//! and column it started at so a later stage can report a position.
//!
//! The rules of §1, and how each is kept here:
//!
//! - **Keywords** are case insensitive: a word is matched after lowercasing,
//!   so `SELECT`, `select` and `SeLeCt` are the same token.
//! - **Identifiers** are case sensitive: `Emp` and `emp` are different names,
//!   and the scanner never lowercases them.
//! - **Strings** are single quoted; two single quotes inside mean one literal
//!   quote, exactly as in Bonus Project 1.
//! - **Numbers** are integers, optionally signed: `-30` is one token.
//! - **Operators** use maximal munch: `<=`, `>=` and `<>` are single tokens.
//! - **Comments** run from `--` to the end of the line and are discarded.
//! - **Statements** are separated by `;`; the scanner produces it as a token.
//!
//! Unlike the Bonus Project 1 scanner, a qualified name is *not* one token:
//! `e.name` scans as `Ident("e") Dot Ident("name")`. SQL's grammar keeps the
//! two parts separate (`column ::= ident | ident "." ident`), and the parser is
//! where they become one qualified reference.

use std::fmt;
use std::iter::Peekable;
use std::str::{Chars, FromStr};

use super::error::SqlError;

// =====================================================================
// Positions
// =====================================================================

/// A place in the input: 1-based line, 0-based column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    /// The 1-based line number.
    pub line: usize,
    /// The 0-based column within `line`.
    pub col: usize,
}

impl fmt::Display for Position {
    /// Renders as `line L, col C`, the form used in error messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, col {}", self.line, self.col)
    }
}

// =====================================================================
// Keywords
// =====================================================================

/// The reserved words of the SQL subset (§1, §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Create,
    Table,
    Insert,
    Into,
    Values,
    Select,
    Distinct,
    From,
    Join,
    On,
    Where,
    Group,
    By,
    Having,
    Order,
    As,
    Asc,
    Desc,
    Limit,
    Union,
    All,
    Intersect,
    Except,
    And,
    Or,
    Not,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Int,
    Varchar,
}

/// Returned by [`Keyword::from_str`] for a word that is not reserved.
pub struct BadKeyword;

impl FromStr for Keyword {
    type Err = BadKeyword;

    /// The keyword `s` spells, case-insensitively, or [`BadKeyword`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let keyword = match s.to_ascii_lowercase().as_str() {
            "create" => Keyword::Create,
            "table" => Keyword::Table,
            "insert" => Keyword::Insert,
            "into" => Keyword::Into,
            "values" => Keyword::Values,
            "select" => Keyword::Select,
            "distinct" => Keyword::Distinct,
            "from" => Keyword::From,
            "join" => Keyword::Join,
            "on" => Keyword::On,
            "where" => Keyword::Where,
            "group" => Keyword::Group,
            "by" => Keyword::By,
            "having" => Keyword::Having,
            "order" => Keyword::Order,
            "as" => Keyword::As,
            "asc" => Keyword::Asc,
            "desc" => Keyword::Desc,
            "limit" => Keyword::Limit,
            "union" => Keyword::Union,
            "all" => Keyword::All,
            "intersect" => Keyword::Intersect,
            "except" => Keyword::Except,
            "and" => Keyword::And,
            "or" => Keyword::Or,
            "not" => Keyword::Not,
            "count" => Keyword::Count,
            "sum" => Keyword::Sum,
            "avg" => Keyword::Avg,
            "min" => Keyword::Min,
            "max" => Keyword::Max,
            "int" => Keyword::Int,
            "varchar" => Keyword::Varchar,
            _ => return Err(BadKeyword),
        };
        Ok(keyword)
    }
}

impl Keyword {
    /// Whether this keyword names an aggregate function.
    pub fn is_aggregate(self) -> bool {
        matches!(
            self,
            Keyword::Count | Keyword::Sum | Keyword::Avg | Keyword::Min | Keyword::Max
        )
    }
}

impl fmt::Display for Keyword {
    /// Renders the keyword as its canonical (upper-case) spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Keyword::Create => f.write_str("CREATE"),
            Keyword::Table => f.write_str("TABLE"),
            Keyword::Insert => f.write_str("INSERT"),
            Keyword::Into => f.write_str("INTO"),
            Keyword::Values => f.write_str("VALUES"),
            Keyword::Select => f.write_str("SELECT"),
            Keyword::Distinct => f.write_str("DISTINCT"),
            Keyword::From => f.write_str("FROM"),
            Keyword::Join => f.write_str("JOIN"),
            Keyword::On => f.write_str("ON"),
            Keyword::Where => f.write_str("WHERE"),
            Keyword::Group => f.write_str("GROUP"),
            Keyword::By => f.write_str("BY"),
            Keyword::Having => f.write_str("HAVING"),
            Keyword::Order => f.write_str("ORDER"),
            Keyword::As => f.write_str("AS"),
            Keyword::Asc => f.write_str("ASC"),
            Keyword::Desc => f.write_str("DESC"),
            Keyword::Limit => f.write_str("LIMIT"),
            Keyword::Union => f.write_str("UNION"),
            Keyword::All => f.write_str("ALL"),
            Keyword::Intersect => f.write_str("INTERSECT"),
            Keyword::Except => f.write_str("EXCEPT"),
            Keyword::And => f.write_str("AND"),
            Keyword::Or => f.write_str("OR"),
            Keyword::Not => f.write_str("NOT"),
            Keyword::Count => f.write_str("COUNT"),
            Keyword::Sum => f.write_str("SUM"),
            Keyword::Avg => f.write_str("AVG"),
            Keyword::Min => f.write_str("MIN"),
            Keyword::Max => f.write_str("MAX"),
            Keyword::Int => f.write_str("INT"),
            Keyword::Varchar => f.write_str("VARCHAR"),
        }
    }
}

// =====================================================================
// Tokens
// =====================================================================

/// The kind of a token; the kinds that carry a value hold it.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// An identifier, case sensitive.
    Ident(String),
    /// An integer literal, possibly negative.
    Int(i32),
    /// A string literal, with its quotes removed and `''` unescaped.
    Str(String),
    /// A reserved word.
    Keyword(Keyword),

    /// `=`.
    Eq,
    /// `<>` (or `!=`).
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,

    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `,`.
    Comma,
    /// `.`.
    Dot,
    /// `*`.
    Star,
    /// `;`.
    Semicolon,
    /// End of input.
    Eof,
}

impl TokenKind {
    /// A short description of the token, for parser error messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident(s) => format!("identifier '{s}'"),
            TokenKind::Int(i) => format!("number {i}"),
            TokenKind::Str(s) => format!("string literal '{s}'"),
            TokenKind::Keyword(kw) => format!("keyword '{kw}'"),
            TokenKind::Eq => "'='".into(),
            TokenKind::Ne => "'<>'".into(),
            TokenKind::Lt => "'<'".into(),
            TokenKind::Le => "'<='".into(),
            TokenKind::Gt => "'>'".into(),
            TokenKind::Ge => "'>='".into(),
            TokenKind::LParen => "'('".into(),
            TokenKind::RParen => "')'".into(),
            TokenKind::Comma => "','".into(),
            TokenKind::Dot => "'.'".into(),
            TokenKind::Star => "'*'".into(),
            TokenKind::Semicolon => "';'".into(),
            TokenKind::Eof => "end of input".into(),
        }
    }

    /// Whether this token is the keyword `kw`.
    pub fn is_keyword(&self, kw: Keyword) -> bool {
        matches!(self, TokenKind::Keyword(k) if *k == kw)
    }
}

/// A token kind plus the position in the input where it started.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Where the token starts.
    pub at: Position,
}

// =====================================================================
// Scanner
// =====================================================================

/// A character-by-character scanner for the SQL subset.
///
/// Tokens are produced one at a time with [`Scanner::next_token`], so the
/// parser can pull the next token only when it needs it. The scanner implements
/// maximal munch and never panics: every failure is a [`SqlError`] of category
/// [`Category::Lexical`](super::error::Category::Lexical) carrying a position.
pub struct Scanner<'a> {
    chars: Peekable<Chars<'a>>,
    line: usize,
    col: usize,
}

impl<'a> Scanner<'a> {
    /// A scanner that has read nothing of `input` yet.
    pub fn new(input: &'a str) -> Self {
        Scanner {
            chars: input.chars().peekable(),
            line: 1,
            col: 0,
        }
    }

    /// The position of the next character.
    fn position(&self) -> Position {
        Position {
            line: self.line,
            col: self.col,
        }
    }

    /// Consume the next character and advance the line/column counters.
    fn bump(&mut self) -> Option<char> {
        let next = self.chars.next();
        if let Some(ch) = next {
            if ch == '\n' {
                self.line += 1;
                self.col = 0;
            } else {
                self.col += ch.len_utf8();
            }
            Some(ch)
        } else {
            None
        }
    }

    /// A lexical error for `ch` at `at`.
    fn unexpected(&self, ch: char, at: Position) -> SqlError {
        SqlError::lexical(format!("unexpected character '{ch}'"), at)
    }

    /// The next token, skipping whitespace and `--` comments. Once the input is
    /// exhausted this keeps returning the end-of-input token.
    ///
    /// Errors: an unterminated string, an integer literal too large for an
    /// `i32`, or a character that starts no token.
    pub fn next_token(&mut self) -> Result<Token, SqlError> {
        loop {
            let at = self.position();
            let Some(c) = self.bump() else {
                return Ok(Token {
                    kind: TokenKind::Eof,
                    at,
                });
            };
            let kind = match c {
                '(' => TokenKind::LParen,
                ')' => TokenKind::RParen,
                ',' => TokenKind::Comma,
                '.' => TokenKind::Dot,
                '*' => TokenKind::Star,
                ';' => TokenKind::Semicolon,
                '=' => TokenKind::Eq,
                '!' => {
                    if self.chars.peek().is_some_and(|&n| n == '=') {
                        self.bump();
                        TokenKind::Ne
                    } else {
                        return Err(self.unexpected('!', at));
                    }
                }
                '<' => match self.chars.peek() {
                    Some(&'=') => {
                        self.bump();
                        TokenKind::Le
                    }
                    Some(&'>') => {
                        self.bump();
                        TokenKind::Ne
                    }
                    _ => TokenKind::Lt,
                },
                '>' => {
                    if self.chars.peek().is_some_and(|&n| n == '=') {
                        self.bump();
                        TokenKind::Ge
                    } else {
                        TokenKind::Gt
                    }
                }
                '\'' => TokenKind::Str(self.scan_string(at)?),
                '-' => match self.chars.peek() {
                    // `--` starts a comment to the end of the line.
                    Some('-') => {
                        self.bump();
                        while let Some(c) = self.bump()
                            && c != '\n'
                        {}
                        continue;
                    }
                    // A signed integer literal.
                    Some(d) if d.is_ascii_digit() => self.scan_number('-', at)?,
                    _ => return Err(self.unexpected('-', at)),
                },
                c if c.is_ascii_digit() => self.scan_number(c, at)?,
                c if is_ident_start(c) => self.scan_word(c),
                c if c.is_ascii_whitespace() => continue,
                other => return Err(self.unexpected(other, at)),
            };
            return Ok(Token { kind, at });
        }
    }

    /// Scan the body of a string literal whose opening quote is at `at`.
    fn scan_string(&mut self, at: Position) -> Result<String, SqlError> {
        let mut literal = String::new();
        loop {
            match self.bump() {
                Some('\'') => {
                    if self.chars.peek().is_some_and(|&n| n == '\'') {
                        self.bump();
                        literal.push('\'');
                    } else {
                        return Ok(literal);
                    }
                }
                Some(c) => literal.push(c),
                None => {
                    return Err(SqlError::lexical("unterminated string literal", at));
                }
            }
        }
    }

    /// Scan an integer literal whose first character is `first`, an optional
    /// sign already consumed from the iterator.
    fn scan_number(&mut self, first: char, at: Position) -> Result<TokenKind, SqlError> {
        let mut literal = String::new();
        literal.push(first);
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                literal.push(c);
                self.bump();
            } else {
                break;
            }
        }
        let value = literal
            .parse::<i32>()
            .map_err(|_| SqlError::lexical("integer literal is out of range", at))?;
        Ok(TokenKind::Int(value))
    }

    /// Scan a word whose first character is `first`: a keyword if reserved, else
    /// an identifier.
    fn scan_word(&mut self, first: char) -> TokenKind {
        let mut word = String::new();
        word.push(first);
        while let Some(&c) = self.chars.peek() {
            if is_ident_continue(c) {
                word.push(c);
                self.bump();
            } else {
                break;
            }
        }
        match word.parse::<Keyword>() {
            Ok(keyword) => TokenKind::Keyword(keyword),
            Err(BadKeyword) => TokenKind::Ident(word),
        }
    }
}

/// Whether `c` may start an identifier: a letter or an underscore.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// Whether `c` may continue an identifier.
fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every token in `input`, up to and including the end-of-input token, with the
/// position each one started at.
///
/// Errors: the first lexical [`SqlError`] the scanner reports.
pub fn tokenize(input: &str) -> Result<Vec<Token>, SqlError> {
    let mut scanner = Scanner::new(input);
    let mut tokens = Vec::new();
    loop {
        let token = scanner.next_token()?;
        let at_eof = token.kind == TokenKind::Eof;
        tokens.push(token);
        if at_eof {
            break;
        }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(input: &str) -> Vec<TokenKind> {
        tokenize(input)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    /// The scanner tracks positions directly, across lines.
    #[test]
    fn positions_track_lines_and_columns() {
        let tokens = tokenize("SELECT a\nFROM R").unwrap();
        assert_eq!(tokens[0].at, Position { line: 1, col: 0 });
        assert_eq!(tokens[1].at, Position { line: 1, col: 7 });
        assert_eq!(tokens[2].at, Position { line: 2, col: 0 });
        assert_eq!(tokens[3].at, Position { line: 2, col: 5 });
    }

    /// Keywords are case insensitive; identifiers are case sensitive.
    #[test]
    fn keyword_case_is_folded_but_identifier_case_is_not() {
        assert!(kinds("select").contains(&TokenKind::Keyword(Keyword::Select)));
        assert!(kinds("SeLeCt").contains(&TokenKind::Keyword(Keyword::Select)));
        assert!(kinds("Emp").contains(&TokenKind::Ident("Emp".into())));
        assert!(!kinds("Emp").contains(&TokenKind::Ident("emp".into())));
    }

    /// A qualified name scans as ident, dot, ident, not one token.
    #[test]
    fn qualified_names_scan_as_three_tokens() {
        assert_eq!(
            kinds("e.name"),
            vec![
                TokenKind::Ident("e".into()),
                TokenKind::Dot,
                TokenKind::Ident("name".into()),
                TokenKind::Eof,
            ]
        );
    }

    /// `<=`, `>=`, `<>` and `!=` are single tokens (maximal munch).
    #[test]
    fn comparison_operators_are_single_tokens() {
        assert_eq!(kinds("<=")[0], TokenKind::Le);
        assert_eq!(kinds(">=")[0], TokenKind::Ge);
        assert_eq!(kinds("<>")[0], TokenKind::Ne);
        assert_eq!(kinds("!=")[0], TokenKind::Ne);
        assert_eq!(kinds("<")[0], TokenKind::Lt);
    }

    /// A doubled quote is one escaped quote and does not close the literal.
    #[test]
    fn doubled_quote_is_an_escaped_quote() {
        assert_eq!(kinds("'O''Brien'")[0], TokenKind::Str("O'Brien".into()));
    }

    /// `--` comments run to the end of the line.
    #[test]
    fn line_comments_are_discarded() {
        assert_eq!(
            kinds("-- a comment\n7"),
            vec![TokenKind::Int(7), TokenKind::Eof]
        );
    }

    /// A signed integer is one token.
    #[test]
    fn signed_integers_are_one_token() {
        assert_eq!(kinds("-30")[0], TokenKind::Int(-30));
    }

    /// An unterminated string is a lexical error with a position.
    #[test]
    fn unterminated_string_reports_a_position() {
        let err = tokenize("SELECT 'abc").unwrap_err();
        assert_eq!(err.category, super::super::error::Category::Lexical);
        assert_eq!((err.at.line, err.at.col), (1, 7));
    }
}
