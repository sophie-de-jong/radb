//! The executor and the context that backs one plan run.
//!
//! [`Executor`] is the public face of a running plan: `Plan::execute` builds
//! the operator tree, opens it, and hands back an executor that owns the run's
//! `Context`. The executor pulls rows through its [`Iterator`] impl, closes the
//! tree once the stream ends — or on drop, if it is abandoned early — and walks
//! it with [`Executor::walk`].
//!
//! Operators never store a reference to the pool — they would fight over the
//! borrow. Instead the executor passes a fresh `&mut Context` into every
//! `open`, `next` and `close`, and the operator uses it for that call only.
//! That is what lets a pull-based tree share one buffer pool.
//!
//! The context also owns the table-to-file-id cache (so two seqscans of the
//! same table share one registered disk) and hands out temporary files, which
//! default to the system temporary directory.
//!
//! The `B`-frame memory account ([`Budget`]) lives here too: the context owns
//! it, sized to the pool's capacity, so the whole plan shares one account and
//! its `B` cannot drift from the pool's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::schema::Schema;
use crate::storage::{BufferPool, FileId, Page, PageId, TableMeta, TempFileDisk};
use crate::value::Row;

use super::error::ExecError;
use super::operators::Operator;

/// The shared state of one plan run.
pub struct Context<'a> {
    /// The buffer pool every base-table page goes through.
    pub pool: &'a mut BufferPool,
    /// The memory account, sized to the pool's capacity.
    pub budget: Budget,
    /// Where the database's data files live.
    db_dir: PathBuf,
    /// Where temporary files are created.
    temp_dir: PathBuf,
    /// Table name to the disk already opened for it.
    files: HashMap<String, FileId>,
}

impl<'a> Context<'a> {
    /// A context over `pool`, with data files in `db_dir` and temporary files
    /// in the system temporary directory.
    ///
    /// The memory account is sized to the pool's capacity, so the plan's `B`
    /// has a single source of truth.
    pub fn new(pool: &'a mut BufferPool, db_dir: PathBuf) -> Self {
        let budget = Budget::new(pool.capacity());
        Context {
            pool,
            budget,
            db_dir,
            temp_dir: std::env::temp_dir(),
            files: HashMap::new(),
        }
    }

    /// Point temporary files at `dir` instead of the system temporary
    /// directory.
    pub fn with_temp_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.temp_dir = dir.into();
        self
    }

    /// The disk holding `meta`'s data, opening and registering it once.
    pub fn file_for(&mut self, meta: &TableMeta) -> Result<FileId, ExecError> {
        if let Some(&file) = self.files.get(&meta.name) {
            return Ok(file);
        }
        let file = self.pool.open_file(self.db_dir.join(&meta.file_name))?;
        self.files.insert(meta.name.clone(), file);
        Ok(file)
    }

    /// Fetch and pin `(file, page)`.
    ///
    /// Returns the pinned id and whether the fetch reached the disk (a miss,
    /// which is what [`Counters::page_reads`](crate::exec::Counters) counts).
    pub fn fetch(&mut self, file: FileId, page: PageId) -> Result<(PageId, bool), ExecError> {
        let before = self.pool.stats().reads;
        match self.pool.fetch_page(file, page) {
            Ok(pinned) => {
                let read = self.pool.stats().reads > before;
                Ok((pinned, read))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Unpin `(file, page)`.
    pub fn unpin(&mut self, file: FileId, page: PageId, is_dirty: bool) -> Result<(), ExecError> {
        self.pool.unpin(file, page, is_dirty)?;
        Ok(())
    }

    /// Borrow a resident page's bytes.
    pub fn page(&self, file: FileId, page: PageId) -> &Page {
        self.pool.page(file, page)
    }

    /// A mutable reference to a resident page's bytes.
    pub fn page_mut(&mut self, file: FileId, page: PageId) -> &mut Page {
        self.pool.page_mut(file, page)
    }

    /// A new temporary file, deleted when it is dropped.
    pub fn temp_file(&self) -> Result<TempFileDisk, ExecError> {
        TempFileDisk::create(&self.temp_dir).map_err(ExecError::Io)
    }

    /// Where temporary files are created.
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }
}

/// The frame account for one plan (§1.2).
///
/// A frame is one page's worth of memory: a buffer-pool pin, or one frame of a
/// blocking operator's own buffers. Every blocking operator takes its frames
/// through [`Budget::acquire`] and gives them back with [`Budget::release`], so
/// [`Budget::peak`] is the largest number of frames the plan ever held at once.
/// The division rule: pipelined operators hold at most one frame — the page a
/// leaf has pinned, or nothing at all for `filter`/`project`/`limit`/`union_all`
/// — and each blocking operator is allowed up to `B` frames. Since a blocking
/// operator drains and closes its child before its blocking phase, the supported
/// plans hold at most `B` frames at once, and `acquire` refuses anything larger
/// rather than silently overrun.
#[derive(Debug)]
pub struct Budget {
    total: usize,
    held: usize,
    peak: usize,
}

impl Budget {
    /// A budget of `total` frames.
    pub fn new(total: usize) -> Self {
        Budget {
            total,
            held: 0,
            peak: 0,
        }
    }

    /// The number of frames the plan is allowed, `B`.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Take up to `frames` frames and return how many were granted.
    ///
    /// A blocking operator asks for its natural requirement, but a plan may
    /// nest two of them: the outer one's child is still holding its frames when
    /// the outer one starts. Rather than fail, the grant is capped at what is
    /// free; an operator that gets fewer frames uses a smaller working set (a
    /// smaller run buffer or fan-in) and still completes. Nothing is ever held
    /// beyond `B`, so §1.2's budget is never exceeded.
    pub fn acquire(&mut self, frames: usize) -> usize {
        let granted = frames.min(self.total.saturating_sub(self.held));
        self.held += granted;
        self.peak = self.peak.max(self.held);
        granted
    }

    /// Give `frames` frames back.
    pub fn release(&mut self, frames: usize) {
        self.held = self.held.saturating_sub(frames);
    }

    /// The number of frames currently held.
    pub fn held(&self) -> usize {
        self.held
    }

    /// The largest number of frames the plan held at once.
    pub fn peak(&self) -> usize {
        self.peak
    }

    /// Record `frames` pages that are live but are owned by an existing
    /// structure rather than acquired through [`Budget::acquire`].
    ///
    /// A lazy final merge keeps one `RunReader` page per run. Those pages are
    /// within the sort's share of `B`, but they must still appear in the peak
    /// report (§5's "largest number of frames held at once").
    pub fn observe(&mut self, frames: usize) {
        self.peak = self.peak.max(self.held + frames);
    }

    /// The frames one sort may use for its run buffers.
    pub fn blocking_buffers(&self) -> usize {
        self.total.max(2)
    }
}

/// One open plan: the operator tree, its shared context, and the pull loop.
///
/// [`Plan::execute`](super::plan::Plan::execute) builds the tree and opens it;
/// the executor owns the run's context from then on, so a caller pulls rows with
/// the [`Iterator`] impl without threading a context through every call. The
/// same tree is walked with [`walk`](Executor::walk). It is closed when the
/// stream ends, or on drop if iteration stops early, so counters read after a
/// drained iteration are final.
///
/// Rows are yielded as `Result`: iteration stops at the first error, and a
/// failed or exhausted executor yields `None` from then on.
pub struct Executor<'a> {
    root: Box<dyn Operator>,
    ctx: Context<'a>,
    /// Set once `next` reaches end of stream or fails, so the iterator fuses.
    finished: bool,
    /// Whether the tree has been closed, so `close` is idempotent.
    closed: bool,
}

impl<'a> Executor<'a> {
    /// Wrap an already-open tree and the context it runs against.
    pub(crate) fn new(root: Box<dyn Operator>, ctx: Context<'a>) -> Self {
        Executor {
            root,
            ctx,
            finished: false,
            closed: false,
        }
    }

    /// The columns of the rows this executor produces.
    pub fn schema(&self) -> &Schema {
        self.root.schema()
    }

    /// The largest number of frames the plan held at once.
    pub fn peak_frames(&self) -> usize {
        self.ctx.budget.peak()
    }

    /// Close the tree, releasing every page and temporary file it holds.
    ///
    /// A blocking operator folds the counters of its final pass in on close,
    /// so counters are only final once the tree is closed. [`Iterator::next`]
    /// closes when the stream ends and [`Drop`] closes a tree abandoned early,
    /// so a caller rarely needs this directly. Idempotent.
    fn close(&mut self) -> Result<(), ExecError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.root.close(&mut self.ctx)
    }

    /// A depth-first walk of the operator tree, in trace order.
    pub fn walk(&self) -> WalkPlan<'_> {
        WalkPlan::new(&*self.root)
    }
}

impl Iterator for Executor<'_> {
    type Item = Result<Row, ExecError>;

    /// Pull one row, or `None` once the tree is exhausted, closed or has
    /// already failed. Reaching the end of the stream closes the tree.
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.closed {
            return None;
        }
        match self.root.next(&mut self.ctx) {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => {
                self.finished = true;
                // The stream ended: close now, releasing every page and
                // finalizing the counters a caller reads after draining us.
                match self.close() {
                    Ok(()) => None,
                    Err(err) => Some(Err(err)),
                }
            }
            Err(err) => {
                self.finished = true;
                Some(Err(err))
            }
        }
    }
}

impl Drop for Executor<'_> {
    /// Close the tree, discarding any error: `Drop` cannot report one.
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// A depth-first walk of an [`Executor`]'s operator tree.
///
/// Yields each operator with its depth — `0` for the root — parents before
/// children and left children before right, the order the trace prints.
pub struct WalkPlan<'a> {
    stack: Vec<(&'a dyn Operator, usize)>,
}

impl<'a> WalkPlan<'a> {
    fn new(root: &'a dyn Operator) -> Self {
        WalkPlan {
            stack: vec![(root, 0)],
        }
    }
}

impl<'a> Iterator for WalkPlan<'a> {
    type Item = (usize, &'a dyn Operator);

    fn next(&mut self) -> Option<Self::Item> {
        let (node, depth) = self.stack.pop()?;
        for child in node.children().into_iter().rev() {
            self.stack.push((child, depth + 1));
        }
        Some((depth, node))
    }
}
