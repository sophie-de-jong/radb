//! The two-cursor merge the set operators share.
//!
//! `intersect` and `except` both sort two inputs into whole-tuple runs collapsed
//! to distinct, then walk the two runs together, one cursor each. They differ
//! only in which tuples they keep: `intersect` keeps a tuple found on both
//! sides, `except` keeps a tuple found on the left only. [`SetMerge`] is that
//! walk, parameterised by [`SetOp`], so the cursor alignment lives in one place.

use std::cmp::Ordering;

use crate::value::Row;

use crate::exec::external_sort::ExternalSort;
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// Which set operation a [`SetMerge`] computes.
#[derive(Clone, Copy, PartialEq)]
enum SetOp {
    /// Keep a tuple once when it is on both sides (`intersect`).
    Intersect,
    /// Keep a tuple once when it is on the left and not the right (`except`).
    Except,
}

/// Two whole-tuple runs, collapsed to distinct, walked by one cursor each.
pub(crate) struct SetMerge {
    /// The left input's whole-tuple run.
    left: ExternalSort,
    /// The right input's whole-tuple run.
    right: ExternalSort,
    /// Current distinct tuple on the left, if any.
    left_cur: Option<Row>,
    /// Current distinct tuple on the right, if any.
    right_cur: Option<Row>,
}

impl SetMerge {
    /// A merge of two inputs whose shapes match.
    pub(crate) fn new(left: &Schema, right: &Schema) -> Self {
        SetMerge {
            // Both sorted runs are materialised, so the right sort is not
            // competing with the left merge's run pages for the budget.
            left: ExternalSort::whole(left).materialized(),
            right: ExternalSort::whole(right).materialized(),
            left_cur: None,
            right_cur: None,
        }
    }

    /// Sort both inputs into distinct runs, closing each as it is drained, then
    /// prime the two cursors.
    pub(crate) fn open(
        &mut self,
        left: &mut dyn Operator,
        right: &mut dyn Operator,
        ctx: &mut Context,
    ) -> Result<(), ExecError> {
        self.left.sort_child(left, ctx)?;
        self.right.sort_child(right, ctx)?;
        self.left_cur = self.left.next_distinct()?;
        self.right_cur = self.right.next_distinct()?;
        Ok(())
    }

    pub(crate) fn next_except(&mut self) -> Result<Option<Row>, ExecError> {
        self.next(SetOp::Except)
    }

    pub(crate) fn next_intersect(&mut self) -> Result<Option<Row>, ExecError> {
        self.next(SetOp::Intersect)
    }

    /// The next tuple `op` keeps, or `None` at end of stream.
    fn next(&mut self, op: SetOp) -> Result<Option<Row>, ExecError> {
        loop {
            match (self.left_cur.take(), self.right_cur.take()) {
                (Some(left), Some(right)) => match self.left.compare(&left, &right)? {
                    // `left` is not on the right: `except` keeps it and
                    // `intersect` does not. Either way, advance the left cursor.
                    Ordering::Less => {
                        self.left_cur = self.left.next_distinct()?;
                        self.right_cur = Some(right);
                        if op == SetOp::Except {
                            return Ok(Some(left));
                        }
                    }
                    // The right cursor is behind; advance it.
                    Ordering::Greater => {
                        self.right_cur = self.right.next_distinct()?;
                        self.left_cur = Some(left);
                    }
                    // On both sides: `intersect` keeps it and `except` does not.
                    // Either way, advance both cursors.
                    Ordering::Equal => {
                        self.left_cur = self.left.next_distinct()?;
                        self.right_cur = self.right.next_distinct()?;
                        if op == SetOp::Intersect {
                            return Ok(Some(left));
                        }
                    }
                },
                // The right side is exhausted: `except` keeps the rest of the
                // left, `intersect` is done.
                (Some(left), None) if op == SetOp::Except => {
                    self.left_cur = self.left.next_distinct()?;
                    return Ok(Some(left));
                }
                _ => return Ok(None),
            }
        }
    }

    /// Settle both runs and return the counters they accumulated.
    pub(crate) fn close(&mut self, ctx: &mut Context) -> Counters {
        self.left.close(ctx);
        self.right.close(ctx);
        let mut counters = self.left.take_counters();
        counters.update(self.right.take_counters());
        counters
    }
}
