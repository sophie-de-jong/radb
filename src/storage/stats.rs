//! Statistics stored in the catalog for query optimization (M4 §1).
//!
//! These are the values `ANALYZE` computes and the optimizer reads. They are
//! owned by the storage layer because the catalog persists them; the analysis
//! algorithm lives in [`crate::optimize::analyze`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Value;
use crate::schema::Schema;
use crate::tree::{DisplayTree, TreeLine};

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

impl fmt::Display for TableStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "table rows={} pages={} width={}",
            self.n_tuples, self.n_pages, self.width
        )
    }
}

impl DisplayTree for TableStats {
    fn children(&self) -> Vec<&dyn DisplayTree> {
        Vec::new()
    }
}

impl fmt::Display for ColumnStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ndv={} min={} max={}", self.ndv, self.min, self.max)
    }
}

impl DisplayTree for ColumnStats {
    fn children(&self) -> Vec<&dyn DisplayTree> {
        self.buckets
            .iter()
            .map(|bucket| bucket as &dyn DisplayTree)
            .collect()
    }
}

impl fmt::Display for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bucket [{}..{}] count={} ndv={}",
            self.min, self.max, self.count, self.ndv
        )
    }
}

impl DisplayTree for Bucket {
    fn children(&self) -> Vec<&dyn DisplayTree> {
        Vec::new()
    }
}

/// A tree for `ANALYZE` output: table counts, then one named column per schema
/// entry with its buckets underneath.
pub fn stats_tree(table: &str, schema: &Schema, stats: &Stats) -> TreeLine {
    let mut root = TreeLine::new(format!("analyze {table}"));
    root.push(TreeLine::from_tree(&stats.table));
    for (column, column_stats) in schema.columns().iter().zip(&stats.columns) {
        let mut node = TreeLine::new(format!("{}: {column_stats}", column.name));
        for bucket in &column_stats.buckets {
            node.push(TreeLine::from_tree(bucket));
        }
        root.push(node);
    }
    root
}
