//! The two structured reports the storage command produces.
//!
//! A `scan` returns a [`ScanReport`] and a `layout` returns a [`PageLayout`].
//! Keeping their construction in the library means the binary only parses
//! arguments and prints the result, while tests and later modules can inspect
//! the same values directly.

use std::fmt;

use crate::schema::Schema;
use crate::value::Row;

use super::BufferStats;

/// One table scan: its rows, shape and buffer counters.
#[derive(Debug, Clone)]
pub struct ScanReport {
    /// The table name.
    pub table: String,
    /// The table's stored schema.
    pub schema: Schema,
    /// The rows the scan produced, in heap order.
    pub rows: Vec<Row>,
    /// The buffer-pool counters accumulated by opening the database and
    /// scanning, including the catalog read (see EXCEPTIONS.md).
    pub stats: BufferStats,
}

impl ScanReport {
    /// The `scan --json` document of M1 §7.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "table": self.table,
            "schema": self.schema.json(),
            "rows": self.rows.iter().map(Row::json).collect::<Vec<_>>(),
            "stats": {
                "hits": self.stats.hits,
                "misses": self.stats.misses,
                "evictions": self.stats.evictions,
                "reads": self.stats.reads,
                "writes": self.stats.writes,
            },
        })
    }
}

/// One slot of a data page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotLayout {
    /// The slot number.
    pub slot: u16,
    /// The record's byte offset, or zero for a dead slot.
    pub offset: u16,
    /// The record's length in bytes.
    pub length: u16,
}

impl SlotLayout {
    /// Whether the slot still refers to a live record.
    pub fn is_live(&self) -> bool {
        self.offset != 0
    }
}

/// The byte-level contents of one slotted data page, as `layout` prints it.
#[derive(Debug, Clone)]
pub struct PageLayout {
    /// The table name.
    pub table: String,
    /// The data file name.
    pub file_name: String,
    /// The page id.
    pub page: u32,
    /// The page id stored in the page header.
    pub page_id: u32,
    /// Number of slots, including dead ones.
    pub slot_count: u16,
    /// Lowest byte offset used by record data.
    pub free_end: u16,
    /// Free bytes between the slot directory and the record data.
    pub free_space: u32,
    /// The slot directory.
    pub slots: Vec<SlotLayout>,
}

impl fmt::Display for PageLayout {
    /// The human-readable `layout` dump, one field or slot per line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "table {} page {} (file {})",
            self.table, self.page, self.file_name
        )?;
        writeln!(f, "offset 0   page_id     = {}", self.page_id)?;
        writeln!(f, "offset 4   slot_count  = {}", self.slot_count)?;
        writeln!(f, "offset 6   free_end    = {}", self.free_end)?;
        writeln!(f, "           free_space  = {}", self.free_space)?;
        writeln!(f, "offset 8   slot directory ({} slots)", self.slot_count)?;
        for slot in &self.slots {
            let state = if slot.is_live() { "live" } else { "dead" };
            writeln!(
                f,
                "  slot {:>4}: offset={:>5} length={:>5} {state}",
                slot.slot, slot.offset, slot.length
            )?;
        }
        writeln!(
            f,
            "free_end..4095: record data ({} bytes)",
            4096usize.saturating_sub(self.free_end as usize)
        )
    }
}
