//! radb — a small relational-algebra query engine.
//!
//! The crate is organized into four modules:
//!
//! - `tokenizer` — positions, lexical errors, tokens and the streaming
//!   `Tokenizer` type;
//! - `parser` — parse errors, the AST, the `Parser` (which owns a
//!   `Tokenizer`), §4.1 relation-definition parsing, and parse-tree
//!   rendering;
//! - `engine` — values, the `Row`/`Relation` types, and the `Engine` that
//!   evaluates an AST bottom-up;
//! - the §8.1 performance-study data generator lives in the `radb-study`
//!   binary (`src/bin/radb_study.rs`).
//!
//! The public API is re-exported at the crate root so consumers (and the
//! test suite) can use `radb::{parse, tokenize, Engine, Relation, ...}`
//! without caring which module owns what.

mod engine;
mod parser;
mod tokenizer;

pub use engine::{Engine, Relation, Row, RowError, SemanticError, Stats, Value};

pub use parser::{
    parse_query, parse_relation, CompareOp, Operand, ParseError, Parser, Predicate, Query,
};

pub use tokenizer::{tokenize, Keyword, LexError, Position, Token, TokenKind, Tokenizer};
