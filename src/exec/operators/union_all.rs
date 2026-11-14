//! `union_all`: the left stream then the right (§2.2).
//!
//! Fully pipelined and duplicate-preserving: it drains the left input, then the
//! right, holding at most one frame.

use crate::schema::Schema;
use crate::value::Row;

use crate::exec::{Context, Counters, ExecError, Operator};

/// `union_all`: the left stream then the right.
pub struct UnionAll {
    /// The left input, drained first.
    left: Box<dyn Operator>,
    /// The right input, drained once the left is exhausted.
    right: Box<dyn Operator>,
    /// Whether the left input is exhausted, so `next` reads the right.
    on_right: bool,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl UnionAll {
    /// A concatenation of `left` and `right`.
    ///
    /// The two inputs must have the same number of columns with the same types,
    /// position by position (their names may differ); the output takes the left
    /// input's names.
    ///
    /// Errors: [`ExecError::Arity`] if the two shapes do not match.
    pub fn new(left: Box<dyn Operator>, right: Box<dyn Operator>) -> Result<Self, ExecError> {
        left.schema().same_shape(right.schema())?;
        Ok(UnionAll {
            left,
            right,
            on_right: false,
            counters: Counters::default(),
        })
    }
}

impl Operator for UnionAll {
    fn schema(&self) -> &Schema {
        self.left.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.left.open(ctx)?;
        self.right.open(ctx)
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        if !self.on_right {
            if let Some(row) = self.left.next(ctx)? {
                self.counters.tuples_out += 1;
                return Ok(Some(row));
            }
            self.on_right = true;
        }
        if let Some(row) = self.right.next(ctx)? {
            self.counters.tuples_out += 1;
            return Ok(Some(row));
        }
        Ok(None)
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.left.close(ctx)?;
        self.right.close(ctx)
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.left, &*self.right]
    }
}

impl std::fmt::Display for UnionAll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("union_all")
    }
}
