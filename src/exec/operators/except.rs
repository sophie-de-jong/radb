//! `except`: set difference (§2.10).
//!
//! A tuple appears once in the output when it is on the left and not the right.
//! Both sides are sorted into distinct runs and merged by [`SetMerge`], which
//! keeps the tuples it finds on the left only.

use crate::value::Row;

use crate::exec::set_merge::SetMerge;
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// `except`: keep a tuple once when it is on the left and not the right.
pub struct Except {
    /// The left input.
    left: Box<dyn Operator>,
    /// The right input.
    right: Box<dyn Operator>,
    /// The two-run merge, and the shape of its result.
    merge: SetMerge,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Except {
    /// A difference of `left` minus `right`.
    ///
    /// The two inputs must have the same number of columns with the same types,
    /// position by position (their names may differ); the output takes the
    /// left input's names.
    ///
    /// Errors: [`ExecError::Arity`] if the two shapes do not match.
    pub fn new(left: Box<dyn Operator>, right: Box<dyn Operator>) -> Result<Self, ExecError> {
        left.schema().same_shape(right.schema())?;
        let merge = SetMerge::new(left.schema(), right.schema());
        Ok(Except {
            left,
            right,
            merge,
            counters: Counters::default(),
        })
    }
}

impl Operator for Except {
    fn schema(&self) -> &Schema {
        self.left.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.merge.open(&mut *self.left, &mut *self.right, ctx)
    }

    fn next(&mut self, _ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        let row = self.merge.next_except()?;
        if row.is_some() {
            self.counters.tuples_out += 1;
        }
        Ok(row)
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.counters.update(self.merge.close(ctx));
        let left = self.left.close(ctx);
        let right = self.right.close(ctx);
        left?;
        right
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.left, &*self.right]
    }
}

impl std::fmt::Display for Except {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("except")
    }
}
