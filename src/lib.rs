//! radb — a database system built one module at a time.
//!
//! The crate is organized into layers that can be imported independently:
//!
//! - [`schema`] — the shared [`Schema`]/[`Column`]/[`ColumnType`] and
//!   [`SemanticError`];
//! - [`value`] — [`Value`] and [`Row`];
//! - [`storage`] — Module 1: pages, heap files, the buffer pool and catalog;
//! - [`exec`] — Module 2: the physical plan and its execution engine;
//! - [`sql`] — Module 3: the SQL lexer, parser, name resolver, the logical
//!   plan it builds, and the streaming compiler. Plans carry typed
//!   [`Condition`] predicates; the string form is only their JSON
//!   representation, decoded by `Condition`'s `Deserialize` at the plan
//!   boundary;
//! - [`optimize`] — Module 4: statistics, rewrites, selectivity, join
//!   enumeration and logical-to-physical conversion;
//! - [`csv`] — the bulk loader's CSV reader;
//! - `database` — [`Database`], the pool + catalog facade that wires the
//!   modules together. It never prints; callers present its results.
//!
//! The public API is re-exported at the crate root so consumers (and the test
//! suite) can use `radb::{Condition, Schema, Value, ...}` without caring which
//! module owns what.

// Public module docs intentionally mention internal engine types such as the
// external sort; linking them in rustdoc would make those types part of the
// public API for no reason.
#![allow(rustdoc::private_intra_doc_links)]

pub mod schema;
pub mod value;

pub mod csv;
pub mod database;
pub mod exec;
pub mod optimize;
pub mod sql;
pub mod storage;

pub use database::{Database, StatementOutcome};
pub use exec::order::Direction;
pub use exec::plan::{Mode, SortKey};
pub use schema::{Column, ColumnType, Schema, SemanticError};
pub use sql::tokenizer::{Keyword, Position, Scanner, Token, TokenKind, tokenize};
pub use sql::{CompareOp, Compiler, Condition, Operand, SqlError, Statement, render_syntax};
pub use value::{Row, Value};
