//! Module 1: the storage engine and buffer pool.
//!
//! The module is layered so each piece can be tested on its own:
//!
//! - [`error`] — every failure mode, with messages that name the offending
//!   table, row, page or slot;
//! - [`codec`] — schema parsing, the record codec (§2, §1.4) over the shared
//!   [`crate::schema`] types, and the catalog binary codec (§6);
//! - [`page`] — the raw 4096-byte buffer, its integer accessors and [`PageId`];
//! - [`slotted`] — the slotted data-page layout (§3);
//! - [`disk`] — the disk manager, its `FileDisk` / `NullDisk` back ends, and
//!   the self-deleting `TempFileDisk` (§1);
//! - [`buffer`] — the buffer pool, [`FileId`], LRU and CLOCK (§5);
//! - [`heap`] — heap files, [`RecordId`], directory and data pages (§4);
//! - [`catalog`] — the `catalog.db` page format (§6);
//! - [`stats`] — the statistics the catalog persists for Module 4.
//!
//! [`Page`] itself makes no assumption about the bytes it holds: each page
//! format owns its own offsets, as free functions over a raw page.
//!
//! The command-line front end is `src/bin/radb_storage.rs`; it parses arguments
//! with clap and prints the reports from [`report`]. The [CSV reader](crate::csv)
//! and every storage operation live in the library so later modules drive the
//! same code paths.

pub mod buffer;
pub mod catalog;
pub mod codec;
pub mod disk;
pub mod error;
pub mod heap;
pub mod page;
pub mod report;
pub mod slotted;
pub mod stats;

pub use buffer::{BufferPool, BufferStats, FileId, Policy};
pub use catalog::{Catalog, TableMeta, validate_table_name};
pub use codec::RecordCodec;
pub use disk::{Disk, FileDisk, NullDisk, TempFileDisk};
pub use error::StorageError;
pub use heap::{HeapCursor, HeapFile, RecordId};
pub use page::{Page, PageId};
pub use report::{PageLayout, ScanReport, SlotLayout};
pub use stats::{Bucket, ColumnStats, Stats, TableStats};
