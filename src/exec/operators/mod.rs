//! The operator layer: the §1.1 iterator interface, the §1.4 counters, and the
//! operators themselves, one per file.
//!
//! Every operator is a [`Operator`]: `open` prepares it and does any blocking
//! work, `next` produces one tuple (or `None` at end of stream), and `close`
//! releases every page and temporary file. Calling `next` on the root pulls one
//! tuple through the whole tree; an operator calls `next` on its children and
//! never reaches past them.
//!
//! The pool is passed in as a `&mut Context` on every call rather than stored,
//! so the whole tree shares one buffer pool.

pub mod distinct;
pub mod except;
pub mod filter;
pub mod group_by;
pub mod hash_join;
pub mod intersect;
pub mod limit;
pub mod nested_loop_join;
pub mod project;
pub mod seq_scan;

pub mod sort;
pub mod sort_merge_join;
pub mod union;
pub mod union_all;

use std::fmt;

use crate::value::Row;

use crate::exec::{Context, ExecError};
use crate::schema::Schema;

/// The four counters of §1.4.
///
/// Each operator owns its own numbers: `tuples_out` is what it returned from
/// `next`, `comparisons` is how many times it evaluated a condition or compared
/// two keys, and `page_reads` / `page_writes` are the disk reads and writes
/// attributed to it (including its own temporary files). A parent never counts
/// its children's work, so the trace shows each operator's own cost.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Tuples this operator returned from `next`.
    pub tuples_out: u64,
    /// Times this operator evaluated a condition or compared two keys.
    pub comparisons: u64,
    /// Disk manager reads attributed to this operator.
    pub page_reads: u64,
    /// Disk manager writes attributed to this operator.
    pub page_writes: u64,
}

impl Counters {
    /// The totals of a whole plan: the `tuples_out` of `rows` (the rows the
    /// root actually produced), plus the summed comparisons and disk reads and
    /// writes of every operator.
    pub fn totals_for(rows: u64, per_node: impl Iterator<Item = Counters>) -> Counters {
        let mut totals = Counters {
            tuples_out: rows,
            ..Counters::default()
        };
        for c in per_node {
            totals.comparisons += c.comparisons;
            totals.page_reads += c.page_reads;
            totals.page_writes += c.page_writes;
        }
        totals
    }

    /// Fold `other`'s comparisons and disk I/O into this operator's counters.
    ///
    /// An operator's `close` uses this to take back the work its
    /// [`ExternalSort`](crate::exec::external_sort::ExternalSort) engine did in
    /// its own counters. `tuples_out` belongs to the operator alone — the
    /// engine produces no tuples — so it is left untouched.
    pub(crate) fn update(&mut self, other: Counters) {
        self.comparisons += other.comparisons;
        self.page_reads += other.page_reads;
        self.page_writes += other.page_writes;
    }
}

impl fmt::Display for Counters {
    /// The trace spelling: `tuples_out=… comparisons=… reads=… writes=…`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tuples_out={} comparisons={} reads={} writes={}",
            self.tuples_out, self.comparisons, self.page_reads, self.page_writes
        )
    }
}

/// A pull-based operator.
///
/// Every operator is also [`fmt::Display`]: its `Display` form is the one-line
/// label the trace prints, for example `seqscan R`.
///
/// An operator whose output has the same shape as its input — `filter`,
/// `limit`, `sort`, `distinct`, `union`, `union_all`, `intersect`, `except` —
/// does not store a schema of its own; it answers [`Operator::schema`] from
/// its child, so the shape has exactly one owner.
///
/// # Lifecycle
///
/// The executor drives only the root; every other rule below is what an
/// operator owes its children and its parent.
///
/// - [`open`](Operator::open) is called **once**, before the first `next`, and
///   top-down: an operator opens each child itself (calling `child.open(ctx)`),
///   because the executor opens nothing but the root. A blocking operator
///   drains its children here — it must finish reading them before `open`
///   returns — while a pipelined operator only forwards `open`.
///   [`Budget`](crate::exec::Budget) frames are claimed here, in the
///   order the memory rule (§1.2) prescribes.
/// - [`next`](Operator::next) is called repeatedly until it returns `None`, and
///   may be called again afterwards: after `None` it keeps returning `None`.
///   It is never called before `open`. It pulls its children with `next` and
///   records work (`tuples_out`, comparisons, its own disk I/O) as it happens.
/// - [`close`](Operator::close) is called **once**, when the stream ends or the
///   executor is dropped, and releases everything `open` acquired: every budget
///   frame, every temporary file, and every child still open. It must be safe
///   after a *failed* `open` (some frames may never have been claimed) and
///   idempotent (safe to call twice), because a failure anywhere closes the
///   already-opened prefix of the tree.
///
/// A blocking operator's `open` drains each child through a sort or a partition
/// and closes it again before returning, so a child is never left open while
/// the operator streams its result. The rule an operator must keep is that by
/// the end of `close` nothing it opened is still open.
pub trait Operator: fmt::Display {
    /// The columns of the tuples this operator produces. Fixed when the
    /// operator is built, so it is valid before `open`.
    fn schema(&self) -> &Schema;

    /// Prepare the operator: open its children, claim budget frames, and do
    /// any blocking work. Called once, before the first `next`; the memory
    /// rule (§1.2) bounds what may be claimed. See [`Operator`] for the
    /// lifecycle rules.
    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError>;

    /// The next tuple, or `None` at end of stream. Called until it first
    /// returns `None`, and may be called again afterwards (it stays `None`).
    /// Never called before `open`.
    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError>;

    /// Release every page and temporary file. Safe to call after a failed
    /// `open`, and safe to call twice. See [`Operator`] for the lifecycle
    /// rules.
    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError>;

    /// This operator's own counters (§1.4).
    fn counters(&self) -> Counters;

    /// This operator's direct children, left to right, for the trace. A leaf
    /// returns an empty vector.
    fn children(&self) -> Vec<&dyn Operator>;
}
