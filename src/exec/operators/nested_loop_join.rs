//! `nested_loop_join`: block nested loop join (§2.7).
//!
//! It takes any condition and is the only join that may serve an inequality. It
//! holds B−2 frames of the outer in memory and streams the inner past it once
//! per outer block; the inner is reopened per block, so its pages are re-read
//! `ceil(M / (B−2))` times. Its children's pages are buffer-pool pins, bounded
//! by the pool's own `B` frames.

use crate::exec::operators::filter::FilterCondition;
use crate::sql::ast::Condition;
use crate::value::{Row, Value};

use crate::exec::join::{BlockInput, BlockNestedLoop};
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::Schema;

/// Block nested loop join.
///
/// `open` claims B frames and reads nothing; `next` fills one B−2-page block
/// of the outer, streams the whole inner past it, then fills the next block.
/// The inner is reopened per block, so its pages are re-read
/// `ceil(M / (B−2))` times, exactly the §2.7 cost.
pub struct NestedLoopJoin {
    /// The condition, compiled to indices over the joined schema.
    filter: FilterCondition,
    /// The joined output shape: the left schema then the right.
    schema: Schema,
    /// The shared §2.7 block loop, over the two child operators.
    block: BlockNestedLoop,
    /// Frames claimed from the budget, released on close.
    frames: usize,
    /// Reused buffer for the joined tuple the condition is tested against, so a
    /// pair costs no allocation beyond the values it copies.
    scratch: Vec<Value>,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl NestedLoopJoin {
    /// A join of `left` and `right` under `condition`.
    ///
    /// The condition may be any predicate, including an inequality; the output
    /// is the left schema then the right.
    ///
    /// Errors: [`ExecError::UnknownColumn`] for a column of neither input, or
    /// [`ExecError::Type`] for a badly typed comparison or duplicate output
    /// names.
    pub fn new(
        left: Box<dyn Operator>,
        right: Box<dyn Operator>,
        condition: &Condition,
    ) -> Result<Self, ExecError> {
        let schema = left.schema().join(right.schema())?;
        let filter = FilterCondition::compile(condition, &schema)?;
        let outer_types = left.schema().column_types();
        let block = BlockNestedLoop::new(
            BlockInput::Operator(left),
            BlockInput::Operator(right),
            outer_types,
        );
        Ok(NestedLoopJoin {
            filter,
            schema,
            block,
            frames: 0,
            scratch: Vec::new(),
            counters: Counters::default(),
        })
    }
}

impl Operator for NestedLoopJoin {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        // The outer is streamed a block at a time; the whole B-frame budget
        // bounds those blocks.
        self.block.open(ctx)?;
        let frames = ctx.budget.total();
        ctx.budget.acquire(frames);
        self.frames = frames;
        Ok(())
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        let Self {
            filter,
            block,
            scratch,
            counters,
            ..
        } = self;
        block.step(ctx, counters, |outer, inner| {
            scratch.clear();
            scratch.extend_from_slice(outer);
            scratch.extend_from_slice(inner);
            Ok(filter.eval(scratch)?)
        })
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.block.close(ctx)?;
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
        self.block.children()
    }
}

impl std::fmt::Display for NestedLoopJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nested_loop_join")
    }
}
