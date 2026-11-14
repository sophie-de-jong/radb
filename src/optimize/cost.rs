//! The page-I/O cost model (M4 §4).

/// One page's size in bytes.
pub const PAGE_SIZE: usize = 4096;

/// The cost model: page I/O as a function of input sizes and the budget.
pub struct CostModel {
    frames: usize,
}

impl CostModel {
    /// A model for a `frames`-frame budget.
    pub fn new(frames: usize) -> Self {
        CostModel {
            frames: frames.max(2),
        }
    }

    /// A sequential scan of `pages` pages.
    pub fn seqscan(&self, pages: u64) -> u64 {
        pages
    }

    /// An external sort of `pages` pages.
    pub fn sort(&self, pages: u64) -> u64 {
        if pages == 0 {
            return 0;
        }
        // A sort needs at least one run buffer and merges at least two runs, so
        // the fan-in is `max(2, B - 1)` and the buffer is at least one page.
        let b = self.frames as f64;
        let fan_in = (b - 1.0).max(2.0);
        let runs = (pages as f64 / b).ceil().max(1.0);
        let passes = (1.0 + runs.log2() / fan_in.log2()).ceil().max(1.0) as u64;
        2 * pages * passes.saturating_sub(1) + pages
    }

    /// A block nested loop join of `left` and `right` pages.
    pub fn nested_loop_join(&self, left: u64, right: u64) -> u64 {
        let b = self.frames;
        if b <= 2 {
            return left + left * right;
        }
        left + ((left as f64) / ((b as f64) - 2.0)).ceil() as u64 * right
    }

    /// A sort-merge join of `left` and `right` pages.
    pub fn sort_merge_join(&self, left: u64, right: u64) -> u64 {
        self.sort(left) + self.sort(right) + left + right
    }

    /// A hash join of `left` and `right` pages.
    pub fn hash_join(&self, left: u64, right: u64) -> u64 {
        3 * (left + right)
    }

    /// The pages `rows` tuples of `width` bytes occupy: `ceil(rows*width/4096)`.
    ///
    /// An empty relation occupies no pages; an estimator that never allows a
    /// row count below one is responsible for keeping this nonzero.
    pub fn pages(&self, rows: u64, width: u32) -> u64 {
        (rows * u64::from(width)).div_ceil(PAGE_SIZE as u64)
    }
}
