//! `intersect`: set intersection (§2.10).
//!
//! A tuple appears once in the output when it appears at least once on both
//! sides. Both sides are sorted into distinct runs and merged by [`SetMerge`],
//! which keeps the tuples it finds on both sides.

use crate::value::Row;

use crate::exec::set_merge::SetMerge;
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// `intersect`: keep a tuple once when it is on both sides.
pub struct Intersect {
    /// The left input.
    left: Box<dyn Operator>,
    /// The right input.
    right: Box<dyn Operator>,
    /// The two-run merge, and the shape of its result.
    merge: SetMerge,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Intersect {
    /// An intersection of `left` and `right`.
    ///
    /// The two inputs must have the same number of columns with the same types,
    /// position by position (their names may differ); the output takes the
    /// left input's names.
    ///
    /// Errors: [`ExecError::Arity`] if the two shapes do not match.
    pub fn new(left: Box<dyn Operator>, right: Box<dyn Operator>) -> Result<Self, ExecError> {
        left.schema().same_shape(right.schema())?;
        let merge = SetMerge::new(left.schema(), right.schema());
        Ok(Intersect {
            left,
            right,
            merge,
            counters: Counters::default(),
        })
    }
}

impl Operator for Intersect {
    fn schema(&self) -> &Schema {
        self.left.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.merge.open(&mut *self.left, &mut *self.right, ctx)
    }

    fn next(&mut self, _ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        let row = self.merge.next_intersect()?;
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

impl std::fmt::Display for Intersect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("intersect")
    }
}
