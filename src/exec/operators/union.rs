//! `union`: concatenate the two inputs and remove duplicates (§2.10).
//!
//! It sorts both inputs together and streams the merged run with adjacent
//! duplicates dropped: the sort spends the whole budget, and the collapse
//! itself holds only the current run page and the previous tuple.

use crate::value::Row;

use crate::exec::external_sort::ExternalSort;
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// `union`: concatenate the two inputs and remove duplicates.
pub struct Union {
    /// The left input.
    left: Box<dyn Operator>,
    /// The right input.
    right: Box<dyn Operator>,
    /// The whole-tuple sort keys and the merged, sorted stream; duplicates
    /// are dropped as it is read.
    sorted: ExternalSort,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Union {
    /// A union of `left` and `right`.
    ///
    /// The two inputs must have the same number of columns with the same types,
    /// position by position (their names may differ); the output takes the left
    /// input's names.
    ///
    /// Errors: [`ExecError::Arity`] if the two shapes do not match.
    pub fn new(left: Box<dyn Operator>, right: Box<dyn Operator>) -> Result<Self, ExecError> {
        left.schema().same_shape(right.schema())?;
        let sorted = ExternalSort::whole(left.schema());
        Ok(Union {
            left,
            right,
            sorted,
            counters: Counters::default(),
        })
    }
}

impl Operator for Union {
    fn schema(&self) -> &Schema {
        self.left.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.left.open(ctx)?;
        self.right.open(ctx)?;
        // Sort the left stream followed by the right as one input.
        let mut on_right = false;
        let sorted = self.sorted.sort_with(
            &mut |ctx| {
                if !on_right {
                    if let Some(row) = self.left.next(ctx)? {
                        return Ok(Some(row));
                    }
                    on_right = true;
                }
                self.right.next(ctx)
            },
            ctx,
        );
        let left_closed = self.left.close(ctx);
        let right_closed = self.right.close(ctx);
        sorted?;
        left_closed?;
        right_closed
    }

    fn next(&mut self, _ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        match self.sorted.next_distinct()? {
            Some(row) => {
                self.counters.tuples_out += 1;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.sorted.close(ctx);
        self.counters.update(self.sorted.take_counters());
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

impl std::fmt::Display for Union {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("union")
    }
}
