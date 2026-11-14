//! `sort_merge_join`: sort merge join over equal keys (§2.8).
//!
//! Both inputs are sorted on their join keys, then two cursors advance in step.
//! A run of equal keys is a *block*: the inner run is spilled to a temp file and
//! re-spanned once per outer row, so an arbitrarily large duplicate run is
//! handled in bounded memory. The merge holds three frames.

use std::cmp::Ordering;

use crate::ColumnType;
use crate::exec::join_condition::JoinCondition;
use crate::sql::ast::Condition;
use crate::value::{Row, Value};

use crate::exec::operators::sort::Sort;
use crate::exec::store::{RunReader, RunWriter};
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// Sort merge join over equal keys.
pub struct SortMergeJoin {
    /// The compiled equality keys.
    condition: JoinCondition,
    /// The joined output shape: the left schema then the right.
    schema: Schema,
    /// The left (outer) input, sorted on the left join keys.
    left: Box<dyn Operator>,
    /// The right (inner) input, sorted on the right join keys.
    right: Box<dyn Operator>,
    /// The right input's types, for spilling its key-run.
    right_types: Vec<ColumnType>,
    /// The current left tuple.
    r: Option<Row>,
    /// The current right tuple.
    s: Option<Row>,
    /// The equal-key block being emitted, if any.
    block: Option<Block>,
    /// Frames claimed from the budget, released on close.
    frames: usize,
    /// §1.4 counters for this operator.
    counters: Counters,
}

/// One equal-key block: an outer row and the spilled inner key-run.
struct Block {
    /// The equal key shared by the block.
    key: Vec<Value>,
    /// The outer row currently being paired with every inner row.
    outer: Row,
    /// The inner key-run, on disk.
    inner: RunReader,
    /// Whether `inner` has been rewound for `outer`.
    primed: bool,
}

impl SortMergeJoin {
    /// A join of `left` and `right` on the equal keys of `condition`.
    ///
    /// Each input is wrapped in a sort on its own join keys, so the caller
    /// passes unsorted children. The output is the left schema then the right.
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
        let right_types = right.schema().column_types();
        let left = Box::new(Sort::ascending_materialized(
            left,
            condition.keys().iter().map(|&(l, _)| l).collect(),
        ));
        let right = Box::new(Sort::ascending_materialized(
            right,
            condition.keys().iter().map(|&(_, r)| r).collect(),
        ));
        Ok(SortMergeJoin {
            condition,
            schema,
            left,
            right,
            right_types,
            r: None,
            s: None,
            block: None,
            frames: 0,
            counters: Counters::default(),
        })
    }

    /// Drop the current block, counting the spill's reads.
    fn finish_block(&mut self) {
        if let Some(block) = self.block.take() {
            self.counters.page_reads += block.inner.reads();
        }
    }

    /// Spill the right key-run equal to `r`'s key and start emitting it.
    fn start_block(&mut self, ctx: &mut Context, r: Row, s: Row) -> Result<(), ExecError> {
        let key = self.condition.left_key(&r)?;
        let mut writer = RunWriter::new(ctx.temp_dir(), self.right_types.clone())?;
        writer.push(&s)?;
        let mut candidate = self.right.next(ctx)?;
        while let Some(c) = &candidate {
            self.counters.comparisons += 1;
            if self.condition.right_key(c)? == key {
                writer.push(c)?;
                candidate = self.right.next(ctx)?;
            } else {
                break;
            }
        }
        self.s = candidate;
        self.r = self.left.next(ctx)?;
        let (inner, writes) = writer.finish()?;
        self.counters.page_writes += writes;
        self.block = Some(Block {
            key,
            outer: r,
            inner,
            primed: false,
        });
        Ok(())
    }

    /// Emit the next pair of the current block, or `None` when it ends.
    fn next_block(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        let inner = {
            let block = self.block.as_mut().expect("a block is present");
            if !block.primed {
                block.inner.rewind();
                block.primed = true;
            }
            block.inner.next()?
        };
        if let Some(inner) = inner {
            let outer = self
                .block
                .as_ref()
                .expect("a block is present")
                .outer
                .clone();
            self.counters.tuples_out += 1;
            return Ok(Some(Row::concat(&outer, &inner)));
        }

        // The inner run is spent: advance the outer row, or leave the block.
        let Some(next_r) = self.r.take() else {
            self.finish_block();
            return Ok(None);
        };
        self.counters.comparisons += 1;
        let same = {
            let block = self.block.as_ref().expect("a block is present");
            self.condition.left_key(&next_r)? == block.key
        };
        if same {
            {
                let block = self.block.as_mut().expect("a block is present");
                block.outer = next_r;
                block.primed = false;
            }
            self.r = self.left.next(ctx)?;
        } else {
            self.r = Some(next_r);
            self.finish_block();
        }
        Ok(None)
    }
}

impl Operator for SortMergeJoin {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.left.open(ctx)?;
        self.right.open(ctx)?;
        // The merge walks two run pages and one output; its sorts' buffers were
        // released when `Sort::open` finished.
        ctx.budget.acquire(3);
        self.frames = 3;
        self.r = self.left.next(ctx)?;
        self.s = self.right.next(ctx)?;
        Ok(())
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        loop {
            if self.block.is_some() {
                if let Some(row) = self.next_block(ctx)? {
                    return Ok(Some(row));
                }
                continue;
            }
            let Some(r) = self.r.take() else {
                return Ok(None);
            };
            let Some(s) = self.s.take() else {
                self.r = Some(r);
                return Ok(None);
            };
            self.counters.comparisons += 1;
            match self.condition.compare(&r, &s)? {
                Ordering::Less => {
                    self.r = self.left.next(ctx)?;
                    self.s = Some(s);
                }
                Ordering::Greater => {
                    self.s = self.right.next(ctx)?;
                    self.r = Some(r);
                }
                Ordering::Equal => self.start_block(ctx, r, s)?,
            }
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.finish_block();
        self.left.close(ctx)?;
        self.right.close(ctx)?;
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

impl std::fmt::Display for SortMergeJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("sort_merge_join")
    }
}
