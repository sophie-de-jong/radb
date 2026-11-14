//! `limit`: the first `n` tuples, then stop pulling (§2.3).
//!
//! Fully pipelined: it passes each tuple through and stops the child once `n`
//! have been produced. It holds at most one frame.

use crate::schema::Schema;
use crate::value::Row;

use crate::exec::{Context, Counters, ExecError, Operator};

/// `limit`: the first `n` tuples, then stop pulling.
pub struct Limit {
    /// The input stream.
    child: Box<dyn Operator>,
    /// How many tuples to emit before stopping the child.
    limit: u64,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Limit {
    /// A limit of `child` to its first `n` tuples.
    pub fn new(child: Box<dyn Operator>, limit: u64) -> Self {
        Limit {
            child,
            limit,
            counters: Counters::default(),
        }
    }
}

impl Operator for Limit {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        if self.counters.tuples_out >= self.limit {
            return Ok(None);
        }
        match self.child.next(ctx)? {
            Some(row) => {
                self.counters.tuples_out += 1;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.child.close(ctx)
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.child]
    }
}

impl std::fmt::Display for Limit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "limit {}", self.limit)
    }
}
