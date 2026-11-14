//! The block nested loop the joins share, and the tuple helper they use.

use crate::ColumnType;
use crate::storage::Page;
use crate::value::Row;

use super::error::ExecError;
use super::store::{RunReader, row_ram_size};
use super::{Context, Counters, Operator};

/// A source a block nested loop reads: a child operator or a spilled run.
pub(crate) enum BlockInput {
    /// A child operator; restarting reopens it.
    Operator(Box<dyn Operator>),
    /// A run on disk; restarting rewinds it.
    Run(Box<RunReader>),
}

impl BlockInput {
    /// Open a child operator; a run needs nothing.
    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        match self {
            BlockInput::Operator(operator) => operator.open(ctx),
            BlockInput::Run(_) => Ok(()),
        }
    }

    /// Start streaming from the beginning: reopen a child, rewind a run.
    fn restart(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        match self {
            BlockInput::Operator(operator) => operator.open(ctx),
            BlockInput::Run(run) => {
                run.rewind();
                Ok(())
            }
        }
    }

    /// The next tuple, or `None` at end of source.
    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        match self {
            BlockInput::Operator(operator) => operator.next(ctx),
            BlockInput::Run(run) => run.next(),
        }
    }

    /// Release the source: close a child, nothing for a run.
    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        match self {
            BlockInput::Operator(operator) => operator.close(ctx),
            BlockInput::Run(_) => Ok(()),
        }
    }

    /// Disk reads this source performed (a run's pages; a child counts its own).
    fn reads(&self) -> u64 {
        match self {
            BlockInput::Run(run) => run.reads(),
            BlockInput::Operator(_) => 0,
        }
    }

    /// The operator behind this source, if it is one.
    fn operator(&self) -> Option<&dyn Operator> {
        match self {
            BlockInput::Operator(operator) => Some(&**operator),
            BlockInput::Run(_) => None,
        }
    }
}

/// The block nested loop (§2.7): read `B − 2` pages of the outer into memory,
/// stream the whole inner past it, and repeat for each outer block.
///
/// It reads either child operators or spilled runs, and its caller supplies the
/// pair test, so `nested_loop_join` and the hash join's overflow fallback share
/// the loop.
pub(crate) struct BlockNestedLoop {
    /// The outer (block) source, read once.
    outer: BlockInput,
    /// The inner source, restarted once per block.
    inner: BlockInput,
    /// The outer's column types, for sizing a block.
    outer_types: Vec<ColumnType>,
    /// The current block of outer tuples held in memory.
    block: Vec<Row>,
    /// Position in `block` of the next outer tuple to pair.
    block_pos: usize,
    /// An outer tuple that did not fit the current block, starting the next.
    pending: Option<Row>,
    /// The inner tuple currently being paired with the block.
    inner_row: Option<Row>,
    /// Whether the outer is exhausted.
    done: bool,
    /// The byte budget for one block (`B − 2` pages), fixed on first use.
    outer_bytes: usize,
}

impl BlockNestedLoop {
    /// A block nested loop over `outer` and `inner`, sizing blocks with
    /// `outer_types`.
    pub(crate) fn new(outer: BlockInput, inner: BlockInput, outer_types: Vec<ColumnType>) -> Self {
        BlockNestedLoop {
            outer,
            inner,
            outer_types,
            block: Vec::new(),
            block_pos: 0,
            pending: None,
            inner_row: None,
            done: false,
            outer_bytes: 0,
        }
    }

    /// Open the outer source.
    pub(crate) fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.outer.open(ctx)
    }

    /// The next joined tuple, or `None` when the loop is exhausted.
    ///
    /// `eval` decides each pair; each pair costs one comparison and each emitted
    /// tuple is counted.
    pub(crate) fn step(
        &mut self,
        ctx: &mut Context,
        counters: &mut Counters,
        mut eval: impl FnMut(&Row, &Row) -> Result<bool, ExecError>,
    ) -> Result<Option<Row>, ExecError> {
        if self.outer_bytes == 0 {
            self.outer_bytes = ctx.budget.total().saturating_sub(2) * Page::SIZE;
        }
        loop {
            if self.block.is_empty() {
                if self.done {
                    return Ok(None);
                }
                self.fill(ctx)?;
                if self.block.is_empty() {
                    self.done = true;
                    return Ok(None);
                }
                self.block_pos = 0;
                self.inner.restart(ctx)?;
                self.inner_row = self.inner.next(ctx)?;
            }
            let Some(inner) = &self.inner_row else {
                // The inner is exhausted for this block: rescan the outer with
                // a fresh inner.
                self.inner.close(ctx)?;
                self.block.clear();
                continue;
            };
            while self.block_pos < self.block.len() {
                let outer = &self.block[self.block_pos];
                counters.comparisons += 1;
                if eval(outer, inner)? {
                    self.block_pos += 1;
                    counters.tuples_out += 1;
                    return Ok(Some(Row::concat(outer, inner)));
                }
                self.block_pos += 1;
            }
            self.block_pos = 0;
            self.inner_row = self.inner.next(ctx)?;
        }
    }

    /// Close both sources.
    pub(crate) fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.outer.close(ctx)?;
        self.inner.close(ctx)
    }

    /// Disk reads both sources performed.
    pub(crate) fn reads(&self) -> u64 {
        self.outer.reads() + self.inner.reads()
    }

    /// The child operators behind the sources (for the trace); a run has none.
    pub(crate) fn children(&self) -> Vec<&dyn Operator> {
        [self.outer.operator(), self.inner.operator()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Fill `block` with the next `B − 2` pages of outer tuples. A row that does
    /// not fit is parked in `pending` and starts the next block.
    fn fill(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        let mut bytes = 0usize;
        if let Some(row) = self.pending.take() {
            bytes = row_ram_size(&row, &self.outer_types)?;
            self.block.push(row);
        }
        while let Some(row) = self.outer.next(ctx)? {
            let size = row_ram_size(&row, &self.outer_types)?;
            if !self.block.is_empty() && bytes + size > self.outer_bytes {
                self.pending = Some(row);
                break;
            }
            bytes += size;
            self.block.push(row);
        }
        Ok(())
    }
}
