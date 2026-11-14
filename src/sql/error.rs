//! Every way the SQL front end can fail, in the one format §6 fixes.
//!
//! Module 3 reports one error and stops, and the error is always
//!
//! ```text
//! error: <category>: <message> at line L, column C
//! ```
//!
//! so the [`Category`] is the single place that decides which of §6's seven
//! words leads the message, and [`SqlError`]'s `Display` is the single place
//! that adds the position. Every stage reports through this type: the lexer
//! (lexical), the parser (syntax), name resolution (name and ambiguity), the
//! type checker (type), the aggregation checker (aggregate) and the
//! schema/shape checks (schema).
//!
//! Positions come from the shared [`super::tokenizer::Position`], whose column
//! is 0-based, so the column a `sql` command prints is 0-based too, exactly as
//! the Bonus Project 1 tokenizer reported it.

use std::error::Error as StdError;
use std::fmt;

use super::tokenizer::Position;

/// One of the seven error categories of §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The character stream itself is malformed.
    Lexical,
    /// The tokens do not form a statement of the grammar.
    Syntax,
    /// A table or column was named that is not in scope.
    Name,
    /// A bare column is present in more than one scope entry.
    Ambiguity,
    /// A type rule was broken: an int against a string, `SUM` over a string.
    Type,
    /// An aggregate was used where SQL forbids one, or a grouping rule broke.
    Aggregate,
    /// A statement is well formed and resolved but its shape is wrong: set
    /// operands with different types, `CREATE TABLE` over an existing table.
    Schema,
}

impl fmt::Display for Category {
    /// The lowercase word the message leads with.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            Category::Lexical => "lexical",
            Category::Syntax => "syntax",
            Category::Name => "name",
            Category::Ambiguity => "ambiguity",
            Category::Type => "type",
            Category::Aggregate => "aggregate",
            Category::Schema => "schema",
        };
        f.write_str(word)
    }
}

/// A failure of any stage of the SQL front end.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlError {
    /// Which of §6's seven categories this belongs to.
    pub category: Category,
    /// What went wrong, without the position.
    pub message: String,
    /// Where in the input it happened.
    pub at: Position,
}

impl SqlError {
    /// An error of `category` at `at`.
    pub fn new(category: Category, message: impl Into<String>, at: Position) -> Self {
        SqlError {
            category,
            message: message.into(),
            at,
        }
    }

    /// A `lexical` error, the category the scanner reports.
    pub fn lexical(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Lexical, message, at)
    }

    /// A `syntax` error, the category the parser reports.
    pub fn syntax(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Syntax, message, at)
    }

    /// A `name` error: an unknown table or column.
    pub fn name(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Name, message, at)
    }

    /// An `ambiguity` error: a bare column in two scope entries.
    pub fn ambiguity(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Ambiguity, message, at)
    }

    /// A `type` error.
    pub fn type_(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Type, message, at)
    }

    /// An `aggregate` error.
    pub fn aggregate(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Aggregate, message, at)
    }

    /// A `schema` error.
    pub fn schema(message: impl Into<String>, at: Position) -> Self {
        Self::new(Category::Schema, message, at)
    }
}

impl fmt::Display for SqlError {
    /// The §6 line: `error: <category>: <message> at line L, column C`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error: {}: {} at line {}, column {}",
            self.category, self.message, self.at.line, self.at.col
        )
    }
}

impl StdError for SqlError {}
