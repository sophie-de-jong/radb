//! Every way the storage engine can fail.
//!
//! Module 1 is graded partly on its error behaviour (§9 of the module
//! specification), so each failure mode gets its own variant and the `Display`
//! text names the offending table, row, page or slot. Nothing here prints; the
//! command-line tool is what turns an error into output.

use std::error::Error as StdError;
use std::fmt;
use std::io;

use crate::storage::slotted;

use super::buffer::FileId;
use super::page::PageId;

/// A failure in the storage engine.
#[derive(Debug)]
pub enum StorageError {
    /// The operating system refused a file operation.
    Io(io::Error),

    /// No frame could be evicted: every frame in the pool is pinned.
    ///
    /// Evicting a pinned page never happens; a fetch that would need to do so
    /// fails with this error instead (§9).
    NoUnpinnedFrame,

    /// `unpin` was called for a page that is not resident.
    UnpinNotResident { file: FileId, page: PageId },

    /// `unpin` was called for a resident page whose pin count is already zero.
    ///
    /// This catches the most common bug in this module (§9).
    UnpinZeroPin { file: FileId, page: PageId },

    /// A record did not fit on an empty page, so it cannot be stored at all.
    ///
    /// The maximum storable record is 4088 bytes; a fresh page has 4088 bytes
    /// of free space and an insert also needs a 4-byte slot, so records longer
    /// than 4084 bytes reach this path. The message names the table and row.
    RecordTooLong { table: String, row: u64, len: usize },

    /// A slot number was past the end of the page's slot directory.
    SlotOutOfRange {
        page: PageId,
        slot: u16,
        slot_count: u16,
    },

    /// A slot was read or deleted whose offset is zero, meaning the record it
    /// referred to was deleted (§9).
    DeadSlot { page: PageId, slot: u16 },

    /// A directory page was expected but the page does not look like one.
    Corrupt { detail: String },

    /// A table named on the command line or in a plan is not in the catalog.
    UnknownTable { name: String },

    /// `create-table` was asked for a name the catalog already holds.
    DuplicateTable { name: String },

    /// A schema string could not be parsed.
    BadSchema { detail: String },

    /// A column type string could not be parsed.
    BadType { detail: String },

    /// A stored record did not match its schema, so it could not be decoded.
    Decode { detail: String },

    /// A replacement policy string was neither `lru` nor `clock`.
    BadPolicy { name: String },

    /// A trace file line was not an unsigned page id.
    BadTraceLine { line: u64, detail: String },

    /// A command-line option value was invalid (for example `--frames 0`).
    BadArg { detail: String },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "io error: {e}"),
            StorageError::NoUnpinnedFrame => {
                write!(f, "no unpinned frame available: every frame is pinned")
            }
            StorageError::UnpinNotResident { file, page } => {
                write!(f, "unpin: page {page} of file {file} is not resident")
            }
            StorageError::UnpinZeroPin { file, page } => {
                write!(f, "unpin: page {page} of file {file} has pin count 0")
            }
            StorageError::RecordTooLong { table, row, len } => write!(
                f,
                "record too long for table '{table}' at row {row}: {len} bytes exceeds the {}-byte limit",
                slotted::MAX_RECORD
            ),
            StorageError::SlotOutOfRange {
                page,
                slot,
                slot_count,
            } => write!(
                f,
                "slot {slot} is past the end of page {page} ({slot_count} slots)"
            ),
            StorageError::DeadSlot { page, slot } => {
                write!(f, "slot {slot} on page {page} is dead (record was deleted)")
            }
            StorageError::Corrupt { detail } => write!(f, "corrupt page: {detail}"),
            StorageError::UnknownTable { name } => write!(f, "unknown table '{name}'"),
            StorageError::DuplicateTable { name } => {
                write!(f, "table '{name}' already exists")
            }
            StorageError::BadSchema { detail } => write!(f, "bad schema: {detail}"),
            StorageError::BadType { detail } => write!(f, "bad column type: {detail}"),
            StorageError::Decode { detail } => write!(f, "cannot decode record: {detail}"),
            StorageError::BadPolicy { name } => {
                write!(f, "unknown replacement policy '{name}' (use lru or clock)")
            }
            StorageError::BadTraceLine { line, detail } => {
                write!(f, "line {line} of the trace file: {detail}")
            }
            StorageError::BadArg { detail } => write!(f, "{detail}"),
        }
    }
}

impl StdError for StorageError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        StorageError::Io(e)
    }
}
