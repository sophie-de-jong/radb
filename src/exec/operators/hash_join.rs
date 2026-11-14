//! `hash_join`: Grace hash join over equal keys (§2.9).
//!
//! Both inputs are partitioned once by `h(key) mod (B−1)`; each partition pair
//! is then probed with an in-memory table. A build side that does not fit is
//! joined with the block nested loop instead, which handles a key skew that
//! re-partitioning could not split.

use std::collections::HashMap;

use crate::ColumnType;
use crate::exec::join::{BlockInput, BlockNestedLoop};
use crate::exec::join_condition::JoinCondition;
use crate::sql::ast::Condition;
use crate::storage::Page;
use crate::value::{Row, Value};

use crate::exec::store::{Partitioner, RunReader, row_ram_size};
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// Grace hash join over equal keys.
pub struct HashJoin {
    /// The compiled equality keys.
    condition: JoinCondition,
    /// The joined output shape: the left schema then the right.
    schema: Schema,
    /// The build input.
    left: Box<dyn Operator>,
    /// The probe input.
    right: Box<dyn Operator>,
    /// The build side's column types, for sizing and spilling.
    left_types: Vec<ColumnType>,
    /// The probe side's column types, for spilling.
    right_types: Vec<ColumnType>,
    /// Partition pairs still waiting to be probed.
    work: Vec<(RunReader, RunReader)>,
    /// The in-memory table for the current pair's build side.
    table: Option<HashMap<Vec<Value>, Vec<Row>>>,
    /// The current pair's probe reader.
    probe: Option<RunReader>,
    /// Build rows matching the current probe row, released one per call.
    pending: std::vec::IntoIter<Row>,
    /// The probe row currently being matched.
    probe_row: Option<Row>,
    /// The block nested loop, when a build side was too large to hash.
    bnl: Option<BlockNestedLoop>,
    /// The number of partitions the inputs are split into.
    partitions: usize,
    /// The cap on the in-memory table.
    table_cap: usize,
    /// Frames claimed from the budget, released on close.
    frames: usize,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl HashJoin {
    /// A join of `left` and `right` on the equal keys of `condition`.
    ///
    /// Errors: [`ExecError::UnknownColumn`] or [`ExecError::Type`] if the
    /// condition is not a conjunction of equalities between the two inputs.
    pub fn new(
        left: Box<dyn Operator>,
        right: Box<dyn Operator>,
        condition: &Condition,
    ) -> Result<Self, ExecError> {
        let condition = JoinCondition::compile(condition, left.schema(), right.schema())?;
        let schema = left.schema().join(right.schema())?;
        let left_types = left.schema().column_types();
        let right_types = right.schema().column_types();
        Ok(HashJoin {
            condition,
            schema,
            left,
            right,
            left_types,
            right_types,
            work: Vec::new(),
            table: None,
            probe: None,
            pending: Vec::new().into_iter(),
            probe_row: None,
            bnl: None,
            partitions: 0,
            table_cap: 0,
            frames: 0,
            counters: Counters::default(),
        })
    }

    /// Spill one input into `partitions` runs on its join keys.
    fn partition(&mut self, ctx: &mut Context, build: bool) -> Result<Vec<RunReader>, ExecError> {
        let (child, keys, types) = if build {
            (
                &mut self.left,
                self.condition.keys().iter().map(|&(l, _)| l).collect(),
                self.left_types.clone(),
            )
        } else {
            (
                &mut self.right,
                self.condition.keys().iter().map(|&(_, r)| r).collect(),
                self.right_types.clone(),
            )
        };
        let mut partitioner = Partitioner::new(ctx, self.partitions, keys, types)?;
        while let Some(row) = child.next(ctx)? {
            partitioner.push(&row)?;
        }
        partitioner.finish(&mut self.counters)
    }
}

impl Operator for HashJoin {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        let frames = ctx.budget.total().max(2);
        self.partitions = frames.saturating_sub(1).max(1);
        self.table_cap = self.partitions * Page::SIZE;
        ctx.budget.acquire(self.partitions);
        self.frames = self.partitions;

        self.left.open(ctx)?;
        let build = self.partition(ctx, true)?;
        self.left.close(ctx)?;
        self.right.open(ctx)?;
        let probe = self.partition(ctx, false)?;
        self.right.close(ctx)?;
        self.work = build.into_iter().zip(probe).collect();
        Ok(())
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        loop {
            if let Some(bnl) = self.bnl.as_mut() {
                let condition = &self.condition;
                match bnl.step(ctx, &mut self.counters, |l, r| Ok(condition.eval(l, r)?))? {
                    Some(row) => return Ok(Some(row)),
                    None => {
                        self.counters.page_reads += bnl.reads();
                        self.bnl = None;
                        // The overflow fallback borrowed the whole budget; give
                        // it back and reserve the partition buffers again for
                        // the remaining pairs.
                        if self.frames > 0 {
                            ctx.budget.release(self.frames);
                        }
                        self.frames = ctx.budget.total().saturating_sub(1).max(1);
                        ctx.budget.acquire(self.frames);
                        continue;
                    }
                }
            }
            if self.table.is_some() {
                if let Some(outer) = self.pending.next() {
                    let inner = self.probe_row.as_ref().expect("a match has its probe row");
                    self.counters.tuples_out += 1;
                    return Ok(Some(Row::concat(&outer, inner)));
                }
                let row = match self.probe.as_mut().expect("a probe reader is set").next()? {
                    Some(row) => row,
                    None => {
                        let reads = self.probe.take().expect("a probe reader is set").reads();
                        self.counters.page_reads += reads;
                        self.table = None;
                        continue;
                    }
                };
                let key = self.condition.right_key(&row)?;
                let matches = self
                    .table
                    .as_ref()
                    .and_then(|table| table.get(&key))
                    .cloned()
                    .unwrap_or_default();
                // Every probe compares one key, whether or not it matched, plus
                // one per matching row in the bucket (§1.4).
                self.counters.comparisons += 1 + matches.len() as u64;
                self.pending = matches.into_iter();
                self.probe_row = Some(row);
                continue;
            }

            let Some((mut build, probe)) = self.work.pop() else {
                return Ok(None);
            };
            let mut rows = Vec::new();
            let mut bytes = 0usize;
            let mut overflow = false;
            while let Some(row) = build.next()? {
                let size = row_ram_size(&row, &self.left_types)?;
                if !rows.is_empty() && bytes + size > self.table_cap {
                    overflow = true;
                    break;
                }
                bytes += size;
                rows.push(row);
            }
            if overflow {
                // A partition that still exceeds the table budget falls back
                // to a block nested loop on that one partition pair. That loop
                // needs the whole budget for its outer block, so hand back the
                // partition buffers first.
                build.rewind();
                if self.frames > 0 {
                    ctx.budget.release(self.frames);
                }
                self.frames = ctx.budget.total();
                ctx.budget.acquire(self.frames);
                self.bnl = Some(BlockNestedLoop::new(
                    BlockInput::Run(Box::new(build)),
                    BlockInput::Run(Box::new(probe)),
                    self.left_types.clone(),
                ));
                continue;
            }
            self.counters.page_reads += build.reads();
            let mut table: HashMap<Vec<Value>, Vec<Row>> = HashMap::new();
            for row in rows {
                table
                    .entry(self.condition.left_key(&row)?)
                    .or_default()
                    .push(row);
            }
            self.table = Some(table);
            self.probe = Some(probe);
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.left.close(ctx)?;
        self.right.close(ctx)?;
        if let Some(probe) = self.probe.take() {
            self.counters.page_reads += probe.reads();
        }
        if let Some(bnl) = self.bnl.take() {
            self.counters.page_reads += bnl.reads();
        }
        self.table = None;
        self.work.clear();
        if self.frames > 0 {
            ctx.budget.release(self.frames);
            self.frames = 0;
        }
        Ok(())
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.left, &*self.right]
    }
}

impl std::fmt::Display for HashJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("hash_join")
    }
}
