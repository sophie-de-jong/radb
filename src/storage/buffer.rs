//! The buffer pool: a fixed number of frames, pinning, dirty tracking and a
//! replacement policy (LRU or CLOCK).
//!
//! The pool is a fixed array of frames; a frame holds one page's bytes, a pin
//! count and a dirty flag, and a page table maps `(file, page_id)` to a frame
//! index (§5). Pages from different files share the same frames under the same
//! memory budget, so the table is keyed by `(file, page_id)` rather than by page
//! id alone.
//!
//! Frames are an internal detail: the public surface is page-oriented, exactly
//! as §5 describes it (`fetch_page(page_id) -> page`, `unpin_page(page_id, ..)`).
//! Callers name a `(FileId, PageId)` and never see a frame index: [`fetch`] and
//! [`new_page`] return the pinned page's id, and [`page`]/[`page_mut`] borrow a
//! resident page's bytes. Because the handle is the page id and not a borrow,
//! two pages can be pinned and used at once.
//!
//! [`fetch`]: BufferPool::fetch_page
//! [`new_page`]: BufferPool::new_page
//! [`page`]: BufferPool::page
//! [`page_mut`]: BufferPool::page_mut
//!
//! Counting rules (§5), which must be exact:
//!
//! * a **hit** is a fetch of a resident page;
//! * a **miss** is a fetch that required a disk read;
//! * an **eviction** is a miss that replaced a resident page, so the first `B`
//!   misses on an empty pool are not evictions;
//! * a **read** is one call to the disk manager's read;
//! * a **write** is one call to its write, whether caused by eviction or by
//!   `flush_all`.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::str::FromStr;

use crate::storage::NullDisk;

use super::disk::{Disk, FileDisk};
use super::error::StorageError;
use super::page::{Page, PageId};

/// Which page to replace when the pool is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Least recently used unpinned frame.
    Lru,
    /// CLOCK: a reference bit per frame and a sweeping hand.
    Clock,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Lru => "lru",
            Policy::Clock => "clock",
        }
    }
}

impl FromStr for Policy {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "lru" => Ok(Policy::Lru),
            "clock" => Ok(Policy::Clock),
            other => Err(StorageError::BadPolicy {
                name: other.to_string(),
            }),
        }
    }
}

/// An index into the buffer pool's list of open files, from `register`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub u32);

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A page's identity: which file, and which page in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageKey {
    /// The file the page belongs to.
    pub file: FileId,
    /// The zero-based page id inside that file.
    pub page: PageId,
}

/// The §8.2 counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BufferStats {
    /// Fetches of a resident page.
    pub hits: u64,
    /// Fetches that required a disk read.
    pub misses: u64,
    /// Misses that replaced a resident page.
    pub evictions: u64,
    /// Calls to the disk manager's read.
    pub reads: u64,
    /// Calls to the disk manager's write.
    pub writes: u64,
}

#[derive(Debug, Clone)]
struct Frame {
    key: Option<PageKey>,
    data: Box<Page>,
    pin: u32,
    dirty: bool,
    /// CLOCK reference bit.
    reference: bool,
    /// LRU timestamp of the last access.
    last_used: u64,
}

impl Frame {
    fn free() -> Self {
        Frame {
            key: None,
            data: Box::new(Page::zeroed()),
            pin: 0,
            dirty: false,
            reference: false,
            last_used: 0,
        }
    }
}

/// The buffer pool.
pub struct BufferPool {
    frames: Vec<Frame>,
    resident: HashMap<PageKey, usize>,
    policy: Policy,
    hand: usize,
    tick: u64,
    max_pinned: u32,
    disks: Vec<Box<dyn Disk>>,
    stats: BufferStats,
}

impl BufferPool {
    /// A pool with `frames` frames and the given replacement `policy`.
    ///
    /// # Panics
    /// Panics if `frames` is zero: a pool that can hold no page cannot serve a
    /// fetch.
    pub fn new(frames: usize, policy: Policy) -> Self {
        assert!(frames > 0, "a buffer pool needs at least one frame");
        BufferPool {
            frames: vec![Frame::free(); frames],
            resident: HashMap::new(),
            policy,
            hand: 0,
            tick: 1,
            disks: Vec::new(),
            stats: BufferStats::default(),
            max_pinned: 0,
        }
    }

    /// Register an already-open disk, returning its file id.
    fn register(&mut self, disk: Box<dyn Disk>) -> FileId {
        let id = FileId(self.disks.len() as u32);
        self.disks.push(disk);
        id
    }

    /// Open a page file (creating it if needed) and register it.
    ///
    /// Errors: an underlying I/O failure.
    pub fn open_file(&mut self, path: impl AsRef<Path>) -> io::Result<FileId> {
        let disk = FileDisk::open(path)?;
        Ok(self.register(Box::new(disk)))
    }

    pub fn open_null(&mut self) -> FileId {
        let disk = NullDisk::default();
        self.register(Box::new(disk))
    }

    /// The counters so far.
    pub fn stats(&self) -> BufferStats {
        self.stats
    }

    /// The largest number of frames ever pinned at once.
    pub fn max_pinned(&self) -> u32 {
        self.max_pinned
    }

    /// The number of frames the pool was created with: its fixed capacity.
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// How many resident frames are pinned right now. A finished plan must
    /// leave this at zero (§1.1): every page it fetched has been unpinned.
    pub fn pinned(&self) -> usize {
        self.frames.iter().filter(|frame| frame.pin > 0).count()
    }

    /// The number of pages in a registered file.
    pub fn num_pages(&self, file: FileId) -> u32 {
        self.disks[file.0 as usize].num_pages()
    }

    /// Borrow the bytes of a resident page.
    ///
    /// # Panics
    /// Panics if `(file, page)` is not resident.
    pub fn page(&self, file: FileId, page: PageId) -> &Page {
        let f = self.frame_of(file, page);
        &self.frames[f].data
    }

    /// A mutable reference to a resident page's bytes.
    ///
    /// # Panics
    /// Panics if `(file, page)` is not resident, as [`BufferPool::page`] does.
    pub fn page_mut(&mut self, file: FileId, page: PageId) -> &mut Page {
        let f = self.frame_of(file, page);
        &mut self.frames[f].data
    }

    /// The frame holding `(file, page)`.
    ///
    /// # Panics
    /// Panics if the page is not resident.
    fn frame_of(&self, file: FileId, page: PageId) -> usize {
        match self.resident.get(&PageKey { file, page }) {
            Some(&f) => f,
            None => panic!("page {page} of file {file} is not resident"),
        }
    }

    /// Fetch `(file, page)` and pin it, returning the id of the pinned page.
    ///
    /// On a hit the frame moves to most-recently-used (LRU) or has its
    /// reference bit set (CLOCK). On a miss a victim is chosen among unpinned
    /// frames; if it held a dirty page that page is written first.
    ///
    /// Errors: [`StorageError::NoUnpinnedFrame`] if every frame is pinned, or
    /// an I/O failure.
    pub fn fetch_page(&mut self, file: FileId, page: PageId) -> Result<PageId, StorageError> {
        let key = PageKey { file, page };
        if let Some(&f) = self.resident.get(&key) {
            self.frames[f].pin += 1;
            self.touch(f);
            self.stats.hits += 1;
            self.note_pinned();
            return Ok(page);
        }

        let f = self.choose_victim().ok_or(StorageError::NoUnpinnedFrame)?;
        self.evict_into(f, true)?;
        let data = self.disks[file.0 as usize].read_page(page)?;
        self.stats.reads += 1;
        self.install(f, key, data, false);
        self.stats.misses += 1;
        self.note_pinned();
        Ok(page)
    }

    /// Allocate a new, zero-filled page in `file`, pin it and mark it dirty.
    ///
    /// The page is not read from disk; nothing is written until it is evicted or
    /// flushed. Evicting a dirty victim here counts a write but not an eviction,
    /// because `new_page` is not a fetch.
    ///
    /// Errors: [`StorageError::NoUnpinnedFrame`] if every frame is pinned.
    pub fn new_page(&mut self, file: FileId) -> Result<PageId, StorageError> {
        let f = self.choose_victim().ok_or(StorageError::NoUnpinnedFrame)?;
        self.evict_into(f, false)?;
        let page = self.disks[file.0 as usize].allocate_page();
        let key = PageKey { file, page };
        self.install(f, key, Page::zeroed(), true);
        self.note_pinned();
        Ok(page)
    }

    /// Unpin `(file, page)`. `is_dirty` marks the frame dirty.
    ///
    /// Errors: [`StorageError::UnpinNotResident`] if the page is not resident,
    /// or [`StorageError::UnpinZeroPin`] if its pin count is already zero (§9).
    pub fn unpin(
        &mut self,
        file: FileId,
        page: PageId,
        is_dirty: bool,
    ) -> Result<(), StorageError> {
        let key = PageKey { file, page };
        let f = *self
            .resident
            .get(&key)
            .ok_or(StorageError::UnpinNotResident { file, page })?;
        if self.frames[f].pin == 0 {
            return Err(StorageError::UnpinZeroPin { file, page });
        }
        self.frames[f].dirty |= is_dirty;
        self.frames[f].pin -= 1;
        Ok(())
    }

    /// Write every dirty frame to disk and clear its dirty flag.
    ///
    /// Errors: an I/O failure.
    pub fn flush_all(&mut self) -> Result<(), StorageError> {
        for f in 0..self.frames.len() {
            if self.frames[f].dirty
                && let Some(key) = self.frames[f].key
            {
                let data = *self.frames[f].data;
                self.disks[key.file.0 as usize].write_page(key.page, &data)?;
                self.stats.writes += 1;
                self.frames[f].dirty = false;
            }
        }
        Ok(())
    }

    /// Drop every resident frame. Dirty frames are discarded, not written:
    /// callers that may hold dirty pages must call [`BufferPool::flush_all`]
    /// first. The §5 counters are left alone (see [`BufferPool::reset_stats`]).
    pub fn reset(&mut self) {
        self.frames = (0..self.frames.len()).map(|_| Frame::free()).collect();
        self.resident.clear();
        self.hand = 0;
        self.tick = 1;
        self.max_pinned = 0;
    }

    /// Zero the §5 counters. Used by `scan` so that its reported numbers cover
    /// only the table scan, not the implementation-defined catalog read.
    pub fn reset_stats(&mut self) {
        self.stats = BufferStats::default();
    }

    /// Replay a trace file's page ids through this pool, fetching and
    /// immediately unpinning each as clean (M1 §7).
    ///
    /// The caller registers the trace's synthetic file first. Blank lines are
    /// ignored; a non-numeric line is [`StorageError::BadTraceLine`] naming the
    /// 1-based line number.
    pub fn replay_trace(&mut self, file: FileId, text: &str) -> Result<(), StorageError> {
        for (index, line) in text.lines().enumerate() {
            let line_no = (index + 1) as u64;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let page: PageId = line.parse().map_err(|_| StorageError::BadTraceLine {
                line: line_no,
                detail: format!("'{line}' is not a page id"),
            })?;
            let pinned = self.fetch_page(file, page)?;
            self.unpin(file, pinned, false)?;
        }
        Ok(())
    }

    /// Record a new high-water mark of pinned frames.
    fn note_pinned(&mut self) {
        let now = self.frames.iter().filter(|f| f.pin > 0).count() as u32;
        if now > self.max_pinned {
            self.max_pinned = now;
        }
    }

    /// Mark a frame as just used: LRU moves it to the front, CLOCK sets its
    /// reference bit.
    fn touch(&mut self, f: usize) {
        self.frames[f].last_used = self.next_tick();
        self.frames[f].reference = true;
    }

    fn next_tick(&mut self) -> u64 {
        let t = self.tick;
        self.tick += 1;
        t
    }

    /// Choose an unpinned victim, or `None` if all frames are pinned.
    fn choose_victim(&mut self) -> Option<usize> {
        match self.policy {
            Policy::Lru => {
                let mut best: Option<(u64, usize)> = None;
                for (i, f) in self.frames.iter().enumerate() {
                    if f.pin > 0 {
                        continue;
                    }
                    let better = match best {
                        None => true,
                        Some((t, _)) => f.last_used < t,
                    };
                    if better {
                        best = Some((f.last_used, i));
                    }
                }
                best.map(|(_, i)| i)
            }
            Policy::Clock => {
                let b = self.frames.len();
                let mut swept = 0usize;
                loop {
                    let f = self.hand;
                    if self.frames[f].pin > 0 {
                        self.hand = (self.hand + 1) % b;
                        swept += 1;
                    } else if self.frames[f].reference {
                        self.frames[f].reference = false;
                        self.hand = (self.hand + 1) % b;
                        swept += 1;
                    } else {
                        self.hand = (self.hand + 1) % b;
                        return Some(f);
                    }
                    if swept > 2 * b {
                        return None;
                    }
                }
            }
        }
    }

    /// Make frame `f` reusable: write its old page if dirty, count an eviction
    /// if it held a resident page and `count_eviction`, and clear it.
    ///
    /// `count_eviction` is false for [`new_page`](Self::new_page), which
    /// displaces a page without the fetch that the spec calls an eviction (§5).
    fn evict_into(&mut self, f: usize, count_eviction: bool) -> Result<(), StorageError> {
        if let Some(old) = self.frames[f].key.take() {
            if self.frames[f].dirty {
                let data = *self.frames[f].data;
                self.disks[old.file.0 as usize].write_page(old.page, &data)?;
                self.stats.writes += 1;
            }
            self.resident.remove(&old);
            if count_eviction {
                self.stats.evictions += 1;
            }
        }
        self.frames[f].pin = 0;
        self.frames[f].dirty = false;
        Ok(())
    }

    /// Install a freshly loaded or allocated page into frame `f`.
    fn install(&mut self, f: usize, key: PageKey, data: Page, dirty: bool) {
        self.frames[f].key = Some(key);
        *self.frames[f].data = data;
        self.frames[f].pin = 1;
        self.frames[f].dirty = dirty;
        self.frames[f].reference = true;
        self.frames[f].last_used = self.next_tick();
        self.resident.insert(key, f);
    }
}

#[cfg(test)]
mod tests {
    use super::super::disk::NullDisk;
    use super::*;

    /// §5.3: the LRU and CLOCK traces must reproduce exactly, B = 3.
    #[test]
    fn worked_policy_example_reproduces() {
        let trace = [1u32, 2, 3, 1, 4, 2];

        let mut lru = BufferPool::new(3, Policy::Lru);
        let f = lru.register(Box::new(NullDisk::default()));
        for &p in &trace {
            lru.fetch_page(f, PageId(p)).unwrap();
            lru.unpin(f, PageId(p), false).unwrap();
        }
        let s = lru.stats();
        assert_eq!((s.hits, s.misses, s.evictions), (1, 5, 2), "LRU totals");
        assert_eq!((s.reads, s.writes), (5, 0));

        let mut clock = BufferPool::new(3, Policy::Clock);
        let f = clock.register(Box::new(NullDisk::default()));
        for &p in &trace {
            clock.fetch_page(f, PageId(p)).unwrap();
            clock.unpin(f, PageId(p), false).unwrap();
        }
        let s = clock.stats();
        assert_eq!((s.hits, s.misses, s.evictions), (2, 4, 1), "CLOCK totals");
        assert_eq!((s.reads, s.writes), (4, 0));
    }

    #[test]
    fn unpinning_a_nonresident_or_zero_pin_page_is_an_error() {
        let mut pool = BufferPool::new(2, Policy::Lru);
        let f = pool.register(Box::new(NullDisk::default()));

        match pool.unpin(f, PageId(3), false) {
            Err(StorageError::UnpinNotResident {
                page: PageId(3), ..
            }) => {}
            other => panic!("expected UnpinNotResident, got {other:?}"),
        }

        pool.fetch_page(f, PageId(3)).unwrap();
        pool.unpin(f, PageId(3), false).unwrap();
        match pool.unpin(f, PageId(3), false) {
            Err(StorageError::UnpinZeroPin {
                page: PageId(3), ..
            }) => {}
            other => panic!("expected UnpinZeroPin, got {other:?}"),
        }
    }

    #[test]
    fn fetching_when_every_frame_is_pinned_fails() {
        let mut pool = BufferPool::new(2, Policy::Lru);
        let f = pool.register(Box::new(NullDisk::default()));
        pool.fetch_page(f, PageId(0)).unwrap();
        pool.fetch_page(f, PageId(1)).unwrap();
        match pool.fetch_page(f, PageId(2)) {
            Err(StorageError::NoUnpinnedFrame) => {}
            other => panic!("expected NoUnpinnedFrame, got {other:?}"),
        }
        // Freeing one frame lets the fetch through.
        pool.unpin(f, PageId(0), false).unwrap();
        assert!(pool.fetch_page(f, PageId(2)).is_ok());
    }

    #[test]
    fn first_b_misses_are_not_evictions() {
        let mut pool = BufferPool::new(4, Policy::Lru);
        let f = pool.register(Box::new(NullDisk::default()));
        for p in 0..4 {
            pool.fetch_page(f, PageId(p)).unwrap();
            pool.unpin(f, PageId(p), false).unwrap();
        }
        let s = pool.stats();
        assert_eq!((s.misses, s.evictions), (4, 0));
        // A fifth distinct page must evict.
        pool.fetch_page(f, PageId(4)).unwrap();
        pool.unpin(f, PageId(4), false).unwrap();
        assert_eq!(pool.stats().evictions, 1);
    }

    /// A `new_page` may displace a clean page, but that is not the fetch a
    /// miss/eviction counts (§5).
    #[test]
    fn new_page_does_not_count_an_eviction_or_a_read() {
        let mut pool = BufferPool::new(1, Policy::Lru);
        let f = pool.register(Box::new(NullDisk::default()));
        pool.fetch_page(f, PageId(0)).unwrap();
        pool.unpin(f, PageId(0), false).unwrap();
        let before = pool.stats();
        pool.new_page(f).unwrap();
        let after = pool.stats();
        assert_eq!(after.evictions, before.evictions);
        assert_eq!(after.reads, before.reads);
        assert_eq!(after.misses, before.misses);
    }

    #[test]
    fn clock_advances_the_hand_past_a_newly_loaded_page() {
        // §5.2: a newly loaded page gets reference bit 1 and the hand advances
        // past it. Reloading 1 and 2 after the sweep must therefore be hits.
        let mut pool = BufferPool::new(3, Policy::Clock);
        let f = pool.register(Box::new(NullDisk::default()));
        for &p in &[1u32, 2, 3, 1, 4] {
            pool.fetch_page(f, PageId(p)).unwrap();
            pool.unpin(f, PageId(p), false).unwrap();
        }
        // Frame holding page 2 was cleared to 0, page 3 too; page 1 was evicted.
        // Fetching 2 and 3 must be hits.
        pool.fetch_page(f, PageId(2)).unwrap();
        pool.unpin(f, PageId(2), false).unwrap();
        pool.fetch_page(f, PageId(3)).unwrap();
        pool.unpin(f, PageId(3), false).unwrap();
        assert_eq!(pool.stats().hits, 3);
    }

    #[test]
    fn flush_writes_dirty_frames_once() {
        let mut pool = BufferPool::new(2, Policy::Lru);
        let f = pool.register(Box::new(NullDisk::default()));
        let page = pool.new_page(f).unwrap();
        pool.page_mut(f, page)[0] = 9;
        pool.unpin(f, page, true).unwrap();
        pool.flush_all().unwrap();
        assert_eq!(pool.stats().writes, 1);
        // A second flush writes nothing.
        pool.flush_all().unwrap();
        assert_eq!(pool.stats().writes, 1);
    }
}
