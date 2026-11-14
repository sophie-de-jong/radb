//! Module 2: the execution engine.
//!
//! Module 2 takes a physical plan (§2) and runs it against Module 1. Every
//! operator obeys the same iterator interface (`open` / `next` / `close` /
//! `counters`, §1.1) and the same memory rule (§1.2).
//!
//! The module is layered so each piece can be read on its own:
//!
//! - [`plan`] — the physical plan JSON and its parser;
//! - [`error`] — every failure mode;
//! - [`operators`] — the [`Operator`] trait, the §1.4 [`Counters`], and the
//!   operators themselves, one per file;
//! - the executor — [`Executor`], the running plan that pulls rows, walks its
//!   tree and owns the shared pool, [`Budget`] and temp directory;
//! - [`external_sort`] — the external merge sort engine every blocking
//!   operator reads from;
//! - [`order`] — the sort keys and directions that engine and its operators
//!   order by;
//! - [`store`] — runs and partitions spilled to disk;
//! - [`join`] — the tuple helper and the block nested loop the joins share;
//! - [`join_condition`] — the compiled equality condition the hash and
//!   sort-merge joins share;
//! - [`set_merge`] — the two-cursor merge `intersect` and `except` share;
//!
//! Every operator's shape is the shared [`crate::schema::Schema`] (§1.3); there
//! is no engine-specific schema type.
//!
//! The command-line front end is `src/bin/radb_exec.rs`; it parses arguments
//! with clap, reads one plan and pulls the tree to completion.

pub mod error;
mod executor;
pub mod external_sort;
pub mod join;
pub mod join_condition;
pub mod operators;
pub mod order;
pub mod plan;
pub mod set_merge;
pub mod store;

pub use crate::schema::{Column, Schema};
pub use error::ExecError;
pub(crate) use executor::Context;
pub use executor::{Budget, Executor, WalkPlan};
pub use operators::{Counters, Operator};
pub use order::Direction;
pub use plan::Plan;
