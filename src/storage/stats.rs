//! Statistics stored in the catalog for query optimization (M4 §1).
//!
//! These are the values `ANALYZE` computes and the optimizer reads. They are
//! owned by the storage layer because the catalog persists them; the analysis
//! algorithm lives in [`crate::optimize::analyze`].

use serde::{Deserialize, Serialize};

use crate::Value;

/// Everything `ANALYZE` stores for one table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Table-level counts.
    pub table: TableStats,
    /// Per-column statistics, in schema order.
    pub columns: Vec<ColumnStats>,
}

/// Table-level statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableStats {
    /// The total number of live tuples.
    pub n_tuples: u64,
    /// The number of data pages.
    pub n_pages: u64,
    /// The average record width in bytes.
    pub width: u32,
}

/// Statistics for one column.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnStats {
    /// The number of distinct values.
    pub ndv: u64,
    /// The minimum value, as the column's own type.
    pub min: Value,
    /// The maximum value, as the column's own type.
    pub max: Value,
    /// The equi-depth histogram, in value order.
    pub buckets: Vec<Bucket>,
}

/// One equi-depth histogram bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// The smallest value in the bucket.
    pub min: Value,
    /// The largest value in the bucket.
    pub max: Value,
    /// The number of tuples in the bucket.
    pub count: u64,
    /// The number of distinct values in the bucket.
    pub ndv: u64,
}
