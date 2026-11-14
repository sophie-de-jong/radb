//! `sort`: external merge sort (§2.5).
//!
//! The sort must handle an input of 20 × B pages, so it never holds more than
//! its share of the budget and it never sorts the whole input at once. The
//! engine is [`ExternalSort`]: `open` drains the child through it and this
//! operator simply streams the resulting run back.

use crate::value::Row;

use crate::exec::external_sort::ExternalSort;
use crate::exec::order::SortOrder;
use crate::exec::plan::SortKey;
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// External merge sort.
pub struct Sort {
    /// The input stream.
    child: Box<dyn Operator>,
    /// The sort keys and the single sorted stream `next` reads — a run, or
    /// the final lazy merge — both owned by the engine.
    sorted: ExternalSort,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Sort {
    /// A sort of `child` by the plan's `keys`, resolved against its schema.
    ///
    /// The output has the child's shape, so it is not stored here.
    ///
    /// Errors: [`ExecError::UnknownColumn`] for a key naming no column.
    pub fn new(child: Box<dyn Operator>, keys: &[SortKey]) -> Result<Self, ExecError> {
        let mut columns = Vec::with_capacity(keys.len());
        let mut directions = Vec::with_capacity(keys.len());
        for key in keys {
            columns.push(child.schema().resolve(key.column())?);
            directions.push(key.direction());
        }
        Ok(Sort {
            sorted: ExternalSort::by(child.schema(), SortOrder::new(columns, directions)),
            child,
            counters: Counters::default(),
        })
    }

    /// A sort of `child` by ascending `keys` (schema indices), used by the
    /// joins that sort their own inputs by their equality keys.
    pub fn ascending(child: Box<dyn Operator>, keys: Vec<usize>) -> Self {
        Sort {
            sorted: ExternalSort::by(child.schema(), SortOrder::ascending(keys)),
            child,
            counters: Counters::default(),
        }
    }

    /// An ascending sort whose final merge is written to a run.
    ///
    /// The sort-merge join uses this for both inputs: while it sorts the second
    /// input, the first input's finished run holds no merge readers, so the
    /// two sorts plus the merge never exceed `B`.
    pub fn ascending_materialized(child: Box<dyn Operator>, keys: Vec<usize>) -> Self {
        Sort {
            sorted: ExternalSort::by(child.schema(), SortOrder::ascending(keys)).materialized(),
            child,
            counters: Counters::default(),
        }
    }
}

impl Operator for Sort {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.sorted.sort_child(&mut *self.child, ctx)
    }

    fn next(&mut self, _ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        match self.sorted.next()? {
            Some(row) => {
                self.counters.tuples_out += 1;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        // The final pass streams from `sorted`; count its reads and the key
        // comparisons it made lazily. `close` settles it, so a stream already
        // drained counts once. The child was closed when `open` drained it.
        self.sorted.close(ctx);
        self.counters.update(self.sorted.take_counters());
        Ok(())
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.child]
    }
}

impl std::fmt::Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("sort")
    }
}
