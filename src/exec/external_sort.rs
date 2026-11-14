//! The external merge sort (§2.5), as [`ExternalSort`] — the engine every
//! sorting operator holds.
//!
//! Six operators sort their input: `sort` streams a sorted input back;
//! `distinct` and `group_by` sort (or re-sort an overflowing hash partition)
//! and collapse it; the set operators sort one or both inputs and merge. Each
//! used to spell out the same pieces itself: the sort keys and column types,
//! the run-generation/merge-pass engine, the streaming state of the collapse,
//! and the fold of the run's lazy counters. [`ExternalSort`] is all of that in one
//! place, so an operator keeps only its own behaviour.
//!
//! One `ExternalSort` lives as long as the operator, built from the operator's
//! schema alone: [`ExternalSort::whole`] orders by the whole tuple, [`ExternalSort::by`]
//! by an explicit [`crate::exec::order::SortOrder`]. That is where the column
//! types runs are encoded with, and the order the merge heap compares by lives
//! now.
//!
//! `open` fills it — [`ExternalSort::sort_child`] drains a child through the
//! sort, [`ExternalSort::sort_with`] sorts any pull stream (a set operator's two
//! inputs as one), and [`ExternalSort::partition_child`] takes the hash path,
//! spilling the child into `B − 1` partitions that [`ExternalSort::next_partition`]
//! hands back one at a time: as rows when they fit in memory, or re-sorted
//! through this same engine when one overflows (the §2.6 fallback).
//!
//! `next` consumes it. The sort itself is the §2.5 running shape:
//!
//! ```text
//! pass 0, run generation
//!     fill B frames with input tuples, sort in memory, write one run
//!     repeat until the input is exhausted          -> R0 = ceil(N / B)
//! pass k, merging
//!     merge B−1 runs at a time (one frame per input run, one for output)
//!     repeat until one run remains
//! ```
//!
//! The final run is not written again: [`ExternalSort::next`] streams it. A merge
//! pass with fewer runs than the fan-in carries the untouched runs forward
//! rather than rewriting them, which is the §2.5 carry-forward strategy. Every
//! counter the sort performs lands on the engine's own §1.4 counters as it
//! happens, or lazily on [`ExternalSort::settle`] when the stream runs dry or the
//! operator closes it early; the operator's `close` folds them into its own
//! with [`ExternalSort::take_counters`].

use std::cell::Cell;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::rc::Rc;

use crate::ColumnType;
use crate::schema::Schema;
use crate::storage::Page;
use crate::value::{Row, Value};

use crate::exec::order::SortOrder;
use crate::exec::store::{Partitioner, RunReader, RunWriter, encode_values};
use crate::exec::{Context, Counters, ExecError, Operator};

/// What [`ExternalSort::next_partition`] found: one hash partition of the input.
pub(crate) enum Partition {
    /// The partition fit in memory. Fold (or dedup) these rows yourself —
    /// they arrive in hash order, so the operator's own table collapses them.
    Rows(Vec<Row>),
    /// It did not fit. The partition was re-sorted with the whole budget and
    /// is now the run that `next`, `next_distinct` and `next_group` stream.
    Sorted,
}

/// The external sort engine behind every blocking operator: the sort keys, the
/// engine that produces runs, and the stream the operator reads.
///
/// An `ExternalSort` is built once from a schema (the operator's constructor)
/// and then driven through three phases, all owned here:
///
/// - **sort** — [`ExternalSort::sort_child`], [`ExternalSort::sort_with`] or
///   [`ExternalSort::partition_child`] do the blocking work of `open`;
/// - **stream** — [`ExternalSort::next`] hands out tuples, [`ExternalSort::next_distinct`]
///   and [`ExternalSort::next_group`] collapse them, [`ExternalSort::next_partition`]
///   loads hash partitions one at a time;
/// - **settle** — [`ExternalSort::settle`] (or [`ExternalSort::close`]) folds the
///   stream's lazy reads and comparisons into this engine's own counters, which
///   the operator takes back with [`ExternalSort::take_counters`] in its `close`.
///
/// The stream is `None` before the first sort and again once it has been
/// settled; reading a settled run simply yields nothing.
pub(crate) struct ExternalSort {
    /// The order it orders by, shared with the merge heap's entries.
    order: Rc<SortOrder>,
    /// The input's column types, used to encode runs.
    types: Vec<ColumnType>,
    /// §1.4 counters this engine accumulates on behalf of its operator: every
    /// run, merge and collapse it drives. The operator takes them back with
    /// [`ExternalSort::take_counters`] in its own `close`.
    counters: Counters,
    /// The sort's final run, streamed one tuple at a time: a run already on
    /// disk, or a lazy merge of the last `B−1` runs (§2.5).
    run: Option<Run>,
    /// Hash mode: the partition runs still to process.
    partitions: Vec<RunReader>,
    /// Hash mode: frames held for the partition buffers.
    acquired: usize,
    /// The last tuple [`ExternalSort::next_distinct`] handed out, so its
    /// duplicates are recognized and dropped.
    prev: Option<Row>,
    /// The first tuple of the next group, parked by [`ExternalSort::next_group`]
    /// when it found a key change.
    pending: Option<Row>,
    /// Write the final merge to a run instead of streaming it lazily. The set
    /// operators and sort-merge join use this so two sorts never hold their
    /// merge readers at the same time.
    materialize_final: bool,
}

/// The stream a sort's final pass reads: a run already on disk, or the lazy
/// merge of the last `B−1` runs (§2.5).
///
/// A written run was compared when it was built, so only a merge reports
/// comparisons; both report the reads the stream still owes.
enum Run {
    /// A run already on disk.
    Reader(Box<RunReader>),
    /// The final merge of several runs, streamed and never written.
    Merge(MergeStream),
}

impl Run {
    /// The next tuple, or `None` at end of stream.
    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        match self {
            Run::Reader(reader) => reader.next(),
            Run::Merge(merge) => merge.next(),
        }
    }

    /// Disk reads this stream performed.
    fn reads(&self) -> u64 {
        match self {
            Run::Reader(reader) => reader.reads(),
            Run::Merge(merge) => merge.reads(),
        }
    }

    /// Key comparisons this stream performed while streaming; a written run
    /// reports none.
    fn comparisons(&self) -> u64 {
        match self {
            Run::Reader(_) => 0,
            Run::Merge(merge) => merge.comparisons(),
        }
    }
}

impl ExternalSort {
    /// An engine over tuples of `shape` ordered by every column ascending — the
    /// whole-tuple order `distinct`, `union`, `intersect` and `except`
    /// collapse by.
    pub(crate) fn whole(shape: &Schema) -> Self {
        Self::by(shape, SortOrder::whole(shape))
    }

    /// An engine over tuples of `shape` ordered by `order`.
    pub(crate) fn by(shape: &Schema, order: SortOrder) -> Self {
        ExternalSort {
            order: Rc::new(order),
            types: shape.column_types(),
            counters: Counters::default(),
            run: None,
            partitions: Vec::new(),
            acquired: 0,
            prev: None,
            pending: None,
            materialize_final: false,
        }
    }

    /// Write the final merge to a run instead of streaming it lazily.
    pub(crate) fn materialized(mut self) -> Self {
        self.materialize_final = true;
        self
    }

    /// Sort a whole child operator, closing it afterwards if the sort fails,
    /// so a child is never left open by a failed `open`.
    ///
    /// The child is drained once, so a blocking operator may sort it while
    /// holding only its own frames.
    pub(crate) fn sort_child(
        &mut self,
        child: &mut dyn Operator,
        ctx: &mut Context,
    ) -> Result<(), ExecError> {
        child.open(ctx)?;
        // Drain and close as one step: whatever the sort does, the child is
        // closed before the error (if any) reaches the caller.
        let sorted = self.sort_with(&mut |ctx| child.next(ctx), ctx);
        let closed = child.close(ctx);
        sorted?;
        closed
    }

    /// Sort the tuples of a pull closure and take the result as the stream.
    ///
    /// The set operators use this to sort two inputs' concatenation in one
    /// sort, so the merge sees both sides under one ordering.
    pub(crate) fn sort_with<F>(&mut self, pull: &mut F, ctx: &mut Context) -> Result<(), ExecError>
    where
        F: FnMut(&mut Context) -> Result<Option<Row>, ExecError>,
    {
        let run = self.sort_all(pull, ctx)?;
        self.install(run);
        Ok(())
    }

    /// Spill a whole child operator into `B − 1` partition runs by the hash of
    /// this run's keys (§2.6).
    ///
    /// The frames this holds for the partition buffers stay acquired until an
    /// overflow releases them in [`ExternalSort::next_partition`], or
    /// [`ExternalSort::close`] releases them.
    pub(crate) fn partition_child(
        &mut self,
        child: &mut dyn Operator,
        ctx: &mut Context,
    ) -> Result<(), ExecError> {
        child.open(ctx)?;
        // Drain and close as one step, exactly as `sort_child` does.
        let filled = self.partition_with(&mut |ctx| child.next(ctx), ctx);
        let closed = child.close(ctx);
        filled?;
        closed
    }

    /// The next hash partition: its rows if it fit in memory, or `Sorted` if
    /// it overflowed and is now the stream. `None` once they are all taken —
    /// including in sort mode, which has none.
    pub(crate) fn next_partition(
        &mut self,
        ctx: &mut Context,
    ) -> Result<Option<Partition>, ExecError> {
        let Some(mut reader) = self.partitions.pop() else {
            return Ok(None);
        };
        let capacity = self.acquired.max(1) * Page::SIZE;
        let (rows, overflow) = reader.read_bounded(&self.types, capacity)?;
        if !overflow {
            self.counters.page_reads += reader.reads();
            return Ok(Some(Partition::Rows(rows)));
        }
        // The partition is bigger than its in-memory table: sort it with the
        // whole budget instead, then take the partition frames back for the
        // partitions still to come.
        self.release_frames(ctx);
        reader.rewind();
        let run = self.sort_all(&mut |_| reader.next(), ctx)?;
        self.counters.page_reads += reader.reads();
        self.install(run);
        let partitions = ctx.budget.total().saturating_sub(1).max(1);
        ctx.budget.acquire(partitions);
        self.acquired = partitions;
        Ok(Some(Partition::Sorted))
    }

    /// The next tuple of the run, duplicates and all; `None` when there is no
    /// stream (yet) or it is exhausted.
    pub(crate) fn next(&mut self) -> Result<Option<Row>, ExecError> {
        match self.run.as_mut() {
            Some(run) => run.next(),
            None => Ok(None),
        }
    }

    /// The next tuple that differs from the one before it.
    ///
    /// Because the run is sorted, equal tuples are adjacent, so this drops
    /// every duplicate except the first — the collapse `distinct`, `union`,
    /// `intersect` and `except` are all built from. Each adjacent pair costs
    /// one comparison on `counters` (§1.4).
    ///
    /// Running dry settles the run, folding its lazy reads and comparisons
    /// into `counters`.
    pub(crate) fn next_distinct(&mut self) -> Result<Option<Row>, ExecError> {
        while let Some(row) = self.next()? {
            if let Some(previous) = &self.prev {
                self.counters.comparisons += 1;
                if self.order.compare_rows(previous, &row)? == Ordering::Equal {
                    continue;
                }
            }
            self.prev = Some(row.clone());
            return Ok(Some(row));
        }
        self.settle();
        Ok(None)
    }

    /// The first tuple of the next key group, folding every tuple of the
    /// group — the first one included — through `fold`.
    ///
    /// The first tuple of the *following* group is parked, so no tuple is
    /// lost between calls and `group_by` only has to supply the fold. Each
    /// non-first tuple of a group costs one comparison on `counters`. `None`
    /// once the run is dry (which settles it) or when there is no stream.
    pub(crate) fn next_group(
        &mut self,
        mut fold: impl FnMut(&Row) -> Result<(), ExecError>,
    ) -> Result<Option<Row>, ExecError> {
        let first = match self.pending.take() {
            Some(row) => row,
            None => match self.next()? {
                Some(row) => row,
                None => {
                    self.settle();
                    return Ok(None);
                }
            },
        };
        fold(&first)?;
        while let Some(row) = self.next()? {
            self.counters.comparisons += 1;
            if self.order.compare_rows(&row, &first)? == Ordering::Equal {
                fold(&row)?;
            } else {
                self.pending = Some(row);
                break;
            }
        }
        Ok(Some(first))
    }

    /// Compare `a` to `b` by this run's keys, counting one comparison on this
    /// engine's counters — what a two-input merge like `intersect` costs per
    /// step.
    pub(crate) fn compare(&mut self, a: &Row, b: &Row) -> Result<Ordering, ExecError> {
        self.counters.comparisons += 1;
        self.order.compare_rows(a, b)
    }

    /// This run's sort keys — what an operator's in-memory collapse (a hash
    /// set or table over the same order) keys its entries by.
    pub(crate) fn keys(&self) -> &[usize] {
        self.order.keys()
    }

    /// Fold the run's lazy reads and key comparisons into `counters`, and
    /// finish it.
    ///
    /// A stream settles when it runs dry — [`ExternalSort::next_distinct`] and
    /// [`ExternalSort::next_group`] do it — and [`ExternalSort::close`] settles a
    /// stream abandoned early. Taking the run out makes the fold idempotent
    /// (a finished stream has nothing left to count) and marks the stream
    /// over, so the operator's next read yields nothing.
    pub(crate) fn settle(&mut self) {
        if let Some(run) = self.run.take() {
            self.counters.page_reads += run.reads();
            self.counters.comparisons += run.comparisons();
        }
    }

    /// Settle the stream, drop the partitions still queued and release the
    /// frames hash mode holds. What an operator's `close` calls; idempotent.
    pub(crate) fn close(&mut self, ctx: &mut Context) {
        self.settle();
        self.partitions.clear();
        self.release_frames(ctx);
        self.prev = None;
        self.pending = None;
    }

    /// Take the §1.4 counters this engine has accumulated, leaving it with
    /// zeroes.
    ///
    /// An operator's `close` folds them into its own counters once. Taking the
    /// numbers — rather than copying them — makes that merge idempotent, so a
    /// second `close` cannot double-count the engine's work.
    pub(crate) fn take_counters(&mut self) -> Counters {
        std::mem::take(&mut self.counters)
    }

    /// Adopt a freshly sorted run as the stream: a new sort starts a new
    /// collapse, so the previous stream's state is gone with the old run.
    fn install(&mut self, run: Run) {
        self.run = Some(run);
        self.prev = None;
        self.pending = None;
    }

    /// The sort itself (§2.5), streaming its result instead of writing the
    /// last pass: pass 0 turns `pull`'s tuples into runs, the merge passes
    /// whittle them down to the one run the stream reads.
    ///
    /// Pass 0 and every merge pass but the last write runs here; the final
    /// pass returns a [`Run::Merge`] over its runs instead (§2.5). The
    /// caller owns the stream's lifecycle through the `ExternalSort` that
    /// installs it — that is what lets a join sort one input, then the other,
    /// while the budget sees one sort at a time.
    fn sort_all<F>(&mut self, pull: &mut F, ctx: &mut Context) -> Result<Run, ExecError>
    where
        F: FnMut(&mut Context) -> Result<Option<Row>, ExecError>,
    {
        // Pass 0: fill the frames we can get of tuples, sort, write a run.
        let want = ctx.budget.blocking_buffers();
        let buffers = ctx.budget.acquire(want);
        let capacity = buffers * Page::SIZE;

        let mut buffer: Vec<Row> = Vec::new();
        let mut buffer_bytes = 0usize;
        let mut runs: Vec<RunReader> = Vec::new();
        while let Some(row) = pull(ctx)? {
            let size = encode_values(&self.types, &row)?.len() + 2;
            if !buffer.is_empty() && buffer_bytes + size > capacity {
                runs.push(self.write_run(&mut buffer, ctx)?);
                buffer_bytes = 0;
            }
            buffer_bytes += size;
            buffer.push(row);
        }
        if !buffer.is_empty() {
            runs.push(self.write_run(&mut buffer, ctx)?);
        }
        ctx.budget.release(buffers);

        // An empty input makes an empty run, so `next` streams nothing.
        if runs.is_empty() {
            let (reader, _) = RunWriter::new(ctx.temp_dir(), self.types.clone())?.finish()?;
            return Ok(Run::Reader(Box::new(reader)));
        }

        // Merge passes. Carry untouched runs forward, and merge fan-in groups
        // into written runs. The pass that would leave a single run is the
        // final pass: it streams instead of writing, so it is returned as a
        // `Run::Merge`.
        let fan = ctx.budget.total().saturating_sub(1).max(2);
        while runs.len() > 1 {
            let mut old = std::mem::take(&mut runs);
            if old.len() <= fan {
                if self.materialize_final {
                    let frames = ctx.budget.acquire(old.len() + 1);
                    let run = self.merge_group(old, ctx)?;
                    ctx.budget.release(frames);
                    return Ok(Run::Reader(Box::new(run)));
                }
                // The lazy merge holds one page per run. That is within `B`,
                // but record it so the reported peak is the real one.
                ctx.budget.observe(old.len());
                return Ok(Run::Merge(MergeStream::new(old, self.order.clone())));
            }
            ctx.budget.acquire(fan + 1);
            let mut next_runs: Vec<RunReader> = Vec::new();
            while !old.is_empty() {
                let take = fan.min(old.len());
                let group: Vec<RunReader> = old.drain(0..take).collect();
                if group.len() == 1 {
                    // Carry the untouched run forward.
                    next_runs.extend(group);
                } else {
                    next_runs.push(self.merge_group(group, ctx)?);
                }
            }
            ctx.budget.release(fan + 1);
            runs = next_runs;
        }

        Ok(Run::Reader(Box::new(
            runs.into_iter()
                .next()
                .expect("pass 0 made at least one run"),
        )))
    }

    /// Sort `buffer` in memory and write it as one run.
    fn write_run(
        &mut self,
        buffer: &mut Vec<Row>,
        ctx: &mut Context,
    ) -> Result<RunReader, ExecError> {
        let mut keyed: Vec<(Vec<Value>, Row)> = buffer
            .drain(..)
            .map(|row| Ok((row.key(self.order.keys())?, row)))
            .collect::<Result<_, ExecError>>()?;
        keyed.sort_by(|a, b| {
            self.counters.comparisons += 1;
            self.order.compare_parts(&a.0, &b.0)
        });
        let mut writer = RunWriter::new(ctx.temp_dir(), self.types.clone())?;
        for (_, row) in &keyed {
            writer.push(row)?;
        }
        let (reader, writes) = writer.finish()?;
        self.counters.page_writes += writes;
        Ok(reader)
    }

    /// Merge `group` readers into one written run, counting every key
    /// comparison.
    fn merge_group(
        &mut self,
        mut group: Vec<RunReader>,
        ctx: &mut Context,
    ) -> Result<RunReader, ExecError> {
        let shared: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let mut heap: BinaryHeap<Reverse<Entry>> = BinaryHeap::new();
        for (index, reader) in group.iter_mut().enumerate() {
            if let Some(row) = reader.next()? {
                let key = row.key(self.order.keys())?;
                heap.push(Reverse(Entry {
                    key,
                    index,
                    row,
                    order: self.order.clone(),
                    shared: shared.clone(),
                }));
            }
        }
        let mut writer = RunWriter::new(ctx.temp_dir(), self.types.clone())?;
        while let Some(Reverse(entry)) = heap.pop() {
            writer.push(&entry.row)?;
            if let Some(row) = group[entry.index].next()? {
                let key = row.key(self.order.keys())?;
                heap.push(Reverse(Entry {
                    key,
                    index: entry.index,
                    row,
                    order: self.order.clone(),
                    shared: shared.clone(),
                }));
            }
        }
        self.counters.comparisons += shared.get();
        let reads: u64 = group.iter().map(RunReader::reads).sum();
        let (reader, writes) = writer.finish()?;
        self.counters.page_reads += reads;
        self.counters.page_writes += writes;
        Ok(reader)
    }

    /// Spill `pull`'s tuples into `partitions` hash runs and queue them.
    fn partition_with<F>(&mut self, pull: &mut F, ctx: &mut Context) -> Result<(), ExecError>
    where
        F: FnMut(&mut Context) -> Result<Option<Row>, ExecError>,
    {
        let partitions = ctx.budget.total().saturating_sub(1).max(1);
        ctx.budget.acquire(partitions);
        self.acquired = partitions;
        let mut partitioner = Partitioner::new(
            ctx,
            partitions,
            self.order.keys().to_vec(),
            self.types.clone(),
        )?;
        while let Some(row) = pull(ctx)? {
            partitioner.push(&row)?;
        }
        self.partitions = partitioner.finish(&mut self.counters)?;
        Ok(())
    }

    /// Release the frames held for the partition buffers.
    fn release_frames(&mut self, ctx: &mut Context) {
        if self.acquired > 0 {
            ctx.budget.release(self.acquired);
            self.acquired = 0;
        }
    }
}

/// One item of the merge heap.
struct Entry {
    key: Vec<Value>,
    /// Which input run it came from.
    index: usize,
    row: Row,
    /// The order.
    order: Rc<SortOrder>,
    /// Where the heap's key comparisons are counted.
    shared: Rc<Cell<u64>>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.shared.set(self.shared.get() + 1);
        self.order.compare_parts(&self.key, &other.key)
    }
}

/// The final merge pass, streamed instead of written (§2.5).
///
/// It owns the last `B−1` runs and a heap of their current tuples; `next`
/// pops the smallest and refills from the run it came from. Nothing is written,
/// so the final pass costs only the reads of the runs it consumes, and every
/// key comparison is counted lazily as it happens.
struct MergeStream {
    /// The input runs, one current tuple each on the heap.
    group: Vec<RunReader>,
    /// The order, shared with the heap entries.
    order: Rc<SortOrder>,
    /// Where the heap's key comparisons are counted.
    shared: Rc<Cell<u64>>,
    /// The current tuple of each run, smallest first.
    heap: BinaryHeap<std::cmp::Reverse<Entry>>,
    /// Whether the first tuple of every run has been read into the heap.
    primed: bool,
}

impl MergeStream {
    /// A lazy merge of `group`, ordered by `order`.
    fn new(group: Vec<RunReader>, order: Rc<SortOrder>) -> Self {
        MergeStream {
            group,
            order,
            shared: Rc::new(Cell::new(0)),
            heap: BinaryHeap::new(),
            primed: false,
        }
    }

    /// Read the first tuple of every run into the heap, if not done already.
    fn prime(&mut self) -> Result<(), ExecError> {
        // `group` and the heap are borrowed together, so prime through indices.
        for index in 0..self.group.len() {
            if let Some(row) = self.group[index].next()? {
                let key = row.key(self.order.keys())?;
                self.heap.push(std::cmp::Reverse(Entry {
                    key,
                    index,
                    row,
                    order: self.order.clone(),
                    shared: self.shared.clone(),
                }));
            }
        }
        self.primed = true;
        Ok(())
    }

    /// The next tuple, or `None` at end of merge.
    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if !self.primed {
            self.prime()?;
        }
        let Some(std::cmp::Reverse(entry)) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(row) = self.group[entry.index].next()? {
            let key = row.key(self.order.keys())?;
            self.heap.push(std::cmp::Reverse(Entry {
                key,
                index: entry.index,
                row,
                order: self.order.clone(),
                shared: self.shared.clone(),
            }));
        }
        Ok(Some(entry.row))
    }

    /// Disk reads this merge performed.
    fn reads(&self) -> u64 {
        self.group.iter().map(RunReader::reads).sum()
    }

    /// Key comparisons this merge performed.
    fn comparisons(&self) -> u64 {
        self.shared.get()
    }
}
