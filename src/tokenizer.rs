//! Tokenizer for the radb query language.
//!
//! Two design decisions from GRAMMAR.md are baked into the lexer:
//!
//! 1. Qualified names (`Emp.DID`) are lexed as a *single*
//!    [`TokenKind::QualIdent`] containing a dot, not as three tokens. This
//!    keeps the grammar simple: anywhere an `Ident` is expected, a qualified
//!    name is also legal.
//! 2. Words that are also keywords (`union`, `and`, ...) always tokenize as
//!    their keyword token, even in a position where an attribute name would
//!    make sense (e.g. `select[union=3](R)`). The *parser* — not the lexer —
//!    is responsible for accepting a keyword token as an attribute name
//!    wherever the grammar expects one (see `Parser::parse_attr_name` and
//!    GRAMMAR.md §"Keywords as attribute names").

use std::error::Error as StdError;
use std::fmt;
use std::iter::Peekable;
use std::str::{CharIndices, FromStr};

// =====================================================================
// Positions
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    /// Convert a byte offset into a (line, column) pair. Columns are
    /// 0-based within the line. Never panics: positions past the end of the
    /// input (e.g. the end-of-input token) are clamped.
    pub fn new(input: &str, position: usize) -> Self {
        let position = position.min(input.len());
        let mut line = 1;
        let mut line_start = 0;
        for (i, b) in input.as_bytes().iter().enumerate() {
            if i >= position {
                break;
            }
            if *b == b'\n' {
                line += 1;
                line_start = i + 1;
            }
        }
        Position {
            line,
            col: position - line_start,
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, col {}", self.line, self.col)
    }
}

// =====================================================================
// Lexical errors
// =====================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum LexError {
    /// A string literal was opened with `'` but never closed.
    /// `at` should point at the opening quote.
    UnterminatedString { at: Position },
    /// A character that cannot start any token.
    UnexpectedChar { ch: char, at: Position },
    /// Integer overflow.
    IntegerOverflow { at: Position },
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexError::UnterminatedString { at } => {
                write!(f, "unterminated string literal starting at {at}")
            }
            LexError::UnexpectedChar { ch, at } => {
                write!(f, "unexpected character '{ch}' at {at}")
            }
            LexError::IntegerOverflow { at } => write!(f, "integer overflow at {at}"),
        }
    }
}
impl StdError for LexError {}

// =====================================================================
// Tokens
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Keyword {
    Select,
    Project,
    Rename,
    Join,
    Union,
    Minus,
    Intersect,
    Times,
    And,
    Or,
    Not,
}

pub struct KeywordError;

impl FromStr for Keyword {
    type Err = KeywordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "select" => Ok(Keyword::Select),
            "project" => Ok(Keyword::Project),
            "rename" => Ok(Keyword::Rename),
            "join" => Ok(Keyword::Join),
            "union" => Ok(Keyword::Union),
            "minus" => Ok(Keyword::Minus),
            "intersect" => Ok(Keyword::Intersect),
            "times" => Ok(Keyword::Times),
            "and" => Ok(Keyword::And),
            "or" => Ok(Keyword::Or),
            "not" => Ok(Keyword::Not),
            _ => Err(KeywordError),
        }
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Keyword::Select => write!(f, "select"),
            Keyword::Project => write!(f, "project"),
            Keyword::Rename => write!(f, "rename"),
            Keyword::Join => write!(f, "join"),
            Keyword::Union => write!(f, "union"),
            Keyword::Minus => write!(f, "minus"),
            Keyword::Intersect => write!(f, "intersect"),
            Keyword::Times => write!(f, "times"),
            Keyword::And => write!(f, "and"),
            Keyword::Or => write!(f, "or"),
            Keyword::Not => write!(f, "not"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    QualIdent(String),
    Int(i64),
    Str(String),
    Keyword(Keyword),

    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,

    Eof,
}

impl TokenKind {
    /// Human-readable description of a token for error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            TokenKind::QualIdent(s) => format!("identifier '{s}'"),
            TokenKind::Ident(s) => format!("identifier '{s}'"),
            TokenKind::Int(i) => format!("number {i}"),
            TokenKind::Str(s) => format!("string literal '{s}'"),
            TokenKind::Keyword(kw) => format!("keyword '{kw}'"),
            TokenKind::Eq => "'='".into(),
            TokenKind::Ne => "'!='".into(),
            TokenKind::Lt => "'<'".into(),
            TokenKind::Le => "'<='".into(),
            TokenKind::Gt => "'>'".into(),
            TokenKind::Ge => "'>='".into(),
            TokenKind::LParen => "'('".into(),
            TokenKind::RParen => "')'".into(),
            TokenKind::LBracket => "'['".into(),
            TokenKind::RBracket => "']'".into(),
            TokenKind::LBrace => "'{'".into(),
            TokenKind::RBrace => "'}'".into(),
            TokenKind::Comma => "','".into(),
            TokenKind::Eof => "end of input".into(),
        }
    }
}

/// A token kind plus the byte offset in the input where it started.
pub struct Token {
    pub kind: TokenKind,
    pub at: usize,
}

// =====================================================================
// Tokenizer
// =====================================================================

/// A character-by-character scanner for the query language.
///
/// Tokens are produced one at a time with [`Tokenizer::next_token`], so a
/// [`crate::parser::Parser`] can pull the next token only when it needs it.
/// The scanner implements maximal munch (on seeing `>` it checks the next
/// character before deciding between `>` and `>=`) and never panics: every
/// failure is reported as a [`LexError`] carrying a [`Position`].
pub struct Tokenizer<'a> {
    input: &'a str,
    chars: Peekable<CharIndices<'a>>,
}

impl<'a> Tokenizer<'a> {
    pub fn new(input: &'a str) -> Self {
        Tokenizer {
            input,
            chars: input.char_indices().peekable(),
        }
    }

    /// The input being scanned (used to turn token offsets into positions).
    pub fn input(&self) -> &'a str {
        self.input
    }

    /// Produce the next token. Once the input is exhausted this always
    /// returns the end-of-input token (positioned at the end of the input).
    pub fn next_token(&mut self) -> Result<Token, LexError> {
        while let Some((at, c)) = self.chars.next() {
            let kind = match c {
                '(' => TokenKind::LParen,
                ')' => TokenKind::RParen,
                '[' => TokenKind::LBracket,
                ']' => TokenKind::RBracket,
                '{' => TokenKind::LBrace,
                '}' => TokenKind::RBrace,
                ',' => TokenKind::Comma,
                '=' => TokenKind::Eq,
                '!' => {
                    if self.chars.peek().is_some_and(|&(_, n)| n == '=') {
                        self.chars.next();
                        TokenKind::Ne
                    } else {
                        return Err(LexError::UnexpectedChar {
                            ch: '!',
                            at: Position::new(self.input, at),
                        });
                    }
                }
                '<' => {
                    if self.chars.peek().is_some_and(|&(_, n)| n == '=') {
                        self.chars.next();
                        TokenKind::Le
                    } else {
                        TokenKind::Lt
                    }
                }
                '>' => {
                    if self.chars.peek().is_some_and(|&(_, n)| n == '=') {
                        self.chars.next();
                        TokenKind::Ge
                    } else {
                        TokenKind::Gt
                    }
                }
                '\'' => {
                    let mut literal = String::new();
                    loop {
                        match self.chars.next() {
                            Some((_, '\'')) => {
                                if self.chars.peek().is_some_and(|&(_, n)| n == '\'') {
                                    self.chars.next();
                                    literal.push('\'');
                                } else {
                                    break;
                                }
                            }
                            Some((_, c)) => literal.push(c),
                            None => {
                                return Err(LexError::UnterminatedString {
                                    at: Position::new(self.input, at),
                                })
                            }
                        }
                    }
                    TokenKind::Str(literal)
                }
                '/' => {
                    // `//` comment: discard through the end of the line.
                    if self.chars.peek().is_some_and(|&(_, n)| n == '/') {
                        self.chars
                            .by_ref()
                            .take_while(|&(_, c)| c != '\n')
                            .for_each(drop);
                        continue;
                    }
                    return Err(LexError::UnexpectedChar {
                        ch: '/',
                        at: Position::new(self.input, at),
                    });
                }
                c if c.is_ascii_digit() || c == '-' => {
                    let mut literal = String::new();
                    literal.push(c);
                    while let Some(&(_, c)) = self.chars.peek() {
                        if c.is_ascii_digit() {
                            literal.push(c);
                            self.chars.next();
                        } else {
                            break;
                        }
                    }
                    let int = literal.parse().map_err(|_| LexError::IntegerOverflow {
                        at: Position::new(self.input, at),
                    })?;
                    TokenKind::Int(int)
                }
                c if c.is_ascii_alphabetic() => {
                    let mut word = String::new();
                    word.push(c);
                    let mut qualified = false;
                    while let Some(&(_, c)) = self.chars.peek() {
                        if c == '.' {
                            if qualified {
                                // Already qualified.
                                return Err(LexError::UnexpectedChar {
                                    ch: c,
                                    at: Position::new(self.input, at),
                                });
                            }
                            word.push('.');
                            self.chars.next();
                            qualified = true;
                        } else if c.is_ascii_alphanumeric() {
                            word.push(c);
                            self.chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Ok(kw) = word.parse() {
                        TokenKind::Keyword(kw)
                    } else if qualified {
                        TokenKind::QualIdent(word)
                    } else {
                        TokenKind::Ident(word)
                    }
                }
                c if c.is_ascii_whitespace() => continue,
                ch => {
                    return Err(LexError::UnexpectedChar {
                        ch,
                        at: Position::new(self.input, at),
                    })
                }
            };
            return Ok(Token { kind, at });
        }
        Ok(Token {
            kind: TokenKind::Eof,
            at: self.input.len(),
        })
    }
}

/// Tokenize the whole input up to and including the end-of-input token.
pub fn tokenize(input: &str) -> Result<Vec<TokenKind>, LexError> {
    let mut tokenizer = Tokenizer::new(input);
    let mut tokens = Vec::new();
    loop {
        let tok = tokenizer.next_token()?;
        let at_eof = tok.kind == TokenKind::Eof;
        tokens.push(tok.kind);
        if at_eof {
            break;
        }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `'abc` starts with an unterminated string at byte 0. This used to panic
    /// in the lexer (`at - 1` underflowed); it must be a clean LexError.
    #[test]
    fn unterminated_string_at_position_zero_is_clean_error() {
        let err = tokenize("'abc").unwrap_err();
        match err {
            LexError::UnterminatedString { at } => {
                assert_eq!((at.line, at.col), (1, 0));
            }
            other => panic!("expected UnterminatedString, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("line"), "got: {msg}");
    }
}
