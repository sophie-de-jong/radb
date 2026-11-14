//! The SQL front end: parse, check, then build a logical plan (§3 of M3).
//!
//! The front end reads SQL text and produces a logical plan, never a physical
//! algorithm. It is split into the layers the specification asks for:
//!
//! - `ast` — the abstract syntax tree, internal to the front end;
//! - `parser` — the hand-written recursive-descent parser, internal;
//! - `resolve` — name resolution and scopes;
//! - [`compiler`] — the streaming parse/check/compile front end;
//! - [`plan`] — the logical plan types and their JSON serialisation;
//! - [`tokenizer`] — the hand-written scanner;
//! - [`error`] — the one error format of §6.

pub mod ast;
pub mod compiler;
pub mod error;
mod parser;
pub mod plan;
pub mod report;
mod resolve;
pub mod tokenizer;

// Re-exports for Modules 2, 3 and 4 to share the predicate type and its JSON
// string codec.
pub use ast::{CompareOp, Condition, Operand};
pub use compiler::{Compiler, Statement};
pub use error::SqlError;
pub use parser::render_syntax;
pub use report::{render_statement, statement_json};
pub use tokenizer::{Keyword, Position, Token, TokenKind, tokenize};
