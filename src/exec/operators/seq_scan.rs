//! `seqscan`: every live tuple of a table, in heap order (§2.1).
//!
//! One data page is pinned at a time; the directory page being walked is pinned
//! too, so the scan holds at most two frames. The walk itself is Module 1's
//! [`HeapCursor`], so the bytes on disk are read exactly the way the storage
//! engine reads them; this operator only adds the driver's counting and schema.
//!
//! Reads are counted from the buffer pool's own read counter, sampled around each
//! [`HeapCursor::next`] call, which is exactly §1.4's rule for `page_reads`: a
//! read counts when a fetch actually reaches the disk.

use crate::schema::Schema;
use crate::storage::{FileId, HeapCursor};
use crate::value::Row;

use crate::exec::{Context, Counters, ExecError, Operator};

/// A streaming heap scan.
pub struct SeqScan {
    /// The table name, as it appears in `Display`.
    table: String,
    /// The data file, already opened and registered with the pool.
    file: FileId,
    /// The output shape: the stored schema under the table qualifier.
    schema: Schema,
    /// The heap walk position, created on `open` and released on `close`.
    cursor: Option<HeapCursor>,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl SeqScan {
    /// A scan of `table`, whose data file is `file` and whose stored schema is
    /// `stored`. The output schema qualifies each column as `qualifier.column`,
    /// where `qualifier` is `alias` if given and `table` otherwise, so a
    /// condition such as `R.a > 5` or `e.a > 5` resolves exactly.
    ///
    /// Errors: [`ExecError::Arity`] if two stored columns collide once
    /// qualified (they cannot, while `table` is the same for all).
    pub fn new(table: String, file: FileId, schema: Schema) -> Result<Self, ExecError> {
        Ok(SeqScan {
            table,
            file,
            schema,
            cursor: None,
            counters: Counters::default(),
        })
    }
}

impl Operator for SeqScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.cursor = Some(HeapCursor::new(ctx.pool, self.file)?);
        Ok(())
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        let cursor = match self.cursor.as_mut() {
            Some(cursor) => cursor,
            None => return Ok(None),
        };
        let before = ctx.pool.stats().reads;
        let row = cursor.next(ctx.pool, &self.schema)?;
        self.counters.page_reads += ctx.pool.stats().reads - before;
        match row {
            Some((_rid, values)) => {
                self.counters.tuples_out += 1;
                Ok(Some(Row::from(values)))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.close(ctx.pool);
        }
        self.cursor = None;
        Ok(())
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        Vec::new()
    }
}

impl std::fmt::Display for SeqScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seqscan {}", self.table)
    }
}
