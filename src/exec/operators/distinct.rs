//! `distinct`: remove duplicate tuples (§2.4).
//!
//! Blocking: it must see every input tuple before it can produce any output, so
//! it has two implementations and the plan picks between them.
//!
//! Sort mode drains the input through [`ExternalSort`] and streams it with
//! adjacent duplicates dropped — equal tuples are adjacent in a sorted run.
//! Hash mode asks the same [`ExternalSort`] for partitions of the input, spilled
//! to disk by the whole tuple, and collapses one at a time: in an in-memory
//! hash set if it fits the budget, or through the sorted stream again if it
//! does not, which bounds the working set for any partition size.
//!
//! The memory rule: a blocking operator is allowed the whole budget. Hash mode
//! holds `B − 1` frames for its partition buffers and its one live in-memory
//! partition; the overflow fallback releases them and sorts with the full `B`.

use std::collections::HashSet;

use crate::value::Row;

use crate::exec::external_sort::{ExternalSort, Partition};
use crate::exec::plan::Mode;
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// `distinct`: remove duplicate tuples (§2.4).
pub struct Distinct {
    /// The input stream.
    child: Box<dyn Operator>,
    /// Whether to collapse by sorting or by hashing.
    mode: Mode,
    /// The whole-tuple sort keys, the partition runs and the sorted stream —
    /// everything both modes of the collapse share.
    sorted: ExternalSort,
    /// Hash mode: one partition that fit in memory, ready to emit.
    emitting: Option<std::vec::IntoIter<Row>>,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Distinct {
    /// A `distinct` of `child`, in the given mode. The output has the child's
    /// shape, so it is not stored here.
    pub fn new(child: Box<dyn Operator>, mode: Mode) -> Self {
        let sorted = ExternalSort::whole(child.schema());
        Distinct {
            child,
            mode,
            sorted,
            emitting: None,
            counters: Counters::default(),
        }
    }
}

impl Operator for Distinct {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        match self.mode {
            Mode::Sort => self.sorted.sort_child(&mut *self.child, ctx),
            Mode::Hash => self.sorted.partition_child(&mut *self.child, ctx),
        }
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        loop {
            // Sort mode (or a hash-mode overflow): stream the sorted run,
            // dropping adjacent equals.
            if let Some(row) = self.sorted.next_distinct()? {
                self.counters.tuples_out += 1;
                return Ok(Some(row));
            }

            // Hash mode: drain the partition currently in memory, then load
            // the next one.
            if let Some(row) = self.emitting.as_mut().and_then(|rows| rows.next()) {
                self.counters.tuples_out += 1;
                return Ok(Some(row));
            }
            match self.sorted.next_partition(ctx)? {
                Some(Partition::Rows(rows)) => {
                    let unique = dedup_in_memory(rows, self.sorted.keys(), &mut self.counters)?;
                    self.emitting = Some(unique.into_iter());
                }
                // It did not fit: the sorted run it became is streamed by
                // `next_distinct` at the top of the loop.
                Some(Partition::Sorted) => {}
                // Sort mode ends here too: it has no partitions to load.
                None => return Ok(None),
            }
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.sorted.close(ctx);
        self.counters.update(self.sorted.take_counters());
        self.emitting = None;
        self.child.close(ctx)
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.child]
    }
}

impl std::fmt::Display for Distinct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("distinct")
    }
}

/// Drop duplicates from one in-memory partition, counting one comparison per
/// tuple.
fn dedup_in_memory(
    rows: Vec<Row>,
    keys: &[usize],
    counters: &mut Counters,
) -> Result<Vec<Row>, ExecError> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for row in rows {
        let key = row.key(keys)?;
        counters.comparisons += 1;
        if seen.insert(key) {
            unique.push(row);
        }
    }
    Ok(unique)
}
