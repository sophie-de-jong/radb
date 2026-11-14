//! Heap files: a linked list of directory pages over slotted data pages.
//!
//! Page 0 of every table file is the first directory page. A directory entry
//! records a data page id and that page's free space, so an insert finds a home
//! by first fit without reading every data page.
//!
//! ```text
//! directory page
//! offset  size  field
//!      0     4  page_id
//!      4     4  next_dir_page (0xFFFFFFFF if none)
//!      8     2  entry_count
//!     10     6  entry 0: data_page_id (4), free_bytes (2)
//!    ...
//! capacity = (4096 - 10) / 6 = 681
//! ```
//!
//! Scans pin at most two pages: the directory page being walked and one data
//! page. The whole table is never materialised.

use std::fmt;

use super::buffer::{BufferPool, FileId};
use super::codec::RecordCodec;
use super::error::StorageError;
use super::page::{Page, PageId};
use super::slotted;
use crate::schema::Schema;
use crate::value::Value;

/// The heap directory header: page_id (4) + next_dir_page (4) + entry_count (2).
pub const DIR_HEADER_SIZE: usize = 10;
/// One directory entry: data_page_id (4) + free_bytes (2).
pub const DIR_ENTRY_SIZE: usize = 6;
/// Directory capacity: `floor((4096 - 10) / 6) = 681`.
pub const DIR_CAPACITY: usize = (Page::SIZE - DIR_HEADER_SIZE) / DIR_ENTRY_SIZE;
/// A sentinel for "no next directory page".
pub const NO_PAGE: PageId = PageId(0xFFFF_FFFF);

/// A record id (`rid`): the page holding a record and the slot on that page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId {
    /// The page holding the record.
    pub page: PageId,
    /// The slot on that page.
    pub slot: u16,
}

impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "page {} slot {}", self.page, self.slot)
    }
}

/// Directory-page operations. They live here rather than in `slotted.rs`
/// because the layout is the heap file's, not the slotted page's, and they take
/// the raw [`Page`] rather than living on it.
///
/// Initialise a page as an empty directory page.
pub fn dir_init(page: &mut Page, page_id: PageId) {
    page.fill(0);
    page.write_u32(0, page_id.0);
    page.write_u32(4, NO_PAGE.0);
    page.write_u16(8, 0);
}

/// The next directory page, or [`NO_PAGE`].
pub fn dir_next(page: &Page) -> PageId {
    PageId(page.read_u32(4))
}

/// Set the next directory page.
pub fn dir_set_next(page: &mut Page, next: PageId) {
    page.write_u32(4, next.0);
}

/// The number of data-page entries.
pub fn dir_count(page: &Page) -> u16 {
    page.read_u16(8)
}

/// Entry `i` as `(data_page_id, free_bytes)`.
pub fn dir_entry(page: &Page, i: u16) -> (PageId, u16) {
    let at = DIR_HEADER_SIZE + DIR_ENTRY_SIZE * i as usize;
    (PageId(page.read_u32(at)), page.read_u16(at + 4))
}

/// Overwrite entry `i`.
pub fn dir_set_entry(page: &mut Page, i: u16, data_page: PageId, free: u16) {
    let at = DIR_HEADER_SIZE + DIR_ENTRY_SIZE * i as usize;
    page.write_u32(at, data_page.0);
    page.write_u16(at + 4, free);
}

/// Append an entry if the directory page has room. Returns whether it fit.
pub fn dir_append(page: &mut Page, data_page: PageId, free: u16) -> bool {
    let count = dir_count(page);
    if count as usize >= DIR_CAPACITY {
        return false;
    }
    dir_set_entry(page, count, data_page, free);
    page.write_u16(8, count + 1);
    true
}

/// A heap file: one per table, holding directory pages and data pages.
pub struct HeapFile {
    /// The buffer-pool file id of this table's `.db` file.
    pub file: FileId,
    /// The table name, for error messages.
    pub table: String,
    /// The shape records are encoded and decoded with.
    pub schema: Schema,
}

impl HeapFile {
    /// A handle to the table's file and schema.
    pub fn new(file: FileId, table: impl Into<String>, schema: Schema) -> Self {
        HeapFile {
            file,
            table: table.into(),
            schema,
        }
    }

    /// Create an empty heap file: allocate and initialise page 0.
    ///
    /// Errors: [`StorageError::NoUnpinnedFrame`] or I/O.
    pub fn create(&self, pool: &mut BufferPool) -> Result<(), StorageError> {
        let page = pool.new_page(self.file)?;
        debug_assert_eq!(page, PageId(0), "a fresh heap file's first page is page 0");
        dir_init(pool.page_mut(self.file, page), page);
        pool.unpin(self.file, page, true)?;
        Ok(())
    }

    /// The number of pages in the table's file.
    pub fn num_pages(&self, pool: &BufferPool) -> u32 {
        pool.num_pages(self.file)
    }

    /// The number of data pages, excluding the directory pages (M4 §1:
    /// `n_pages(T)`).
    ///
    /// Errors: an I/O or pin error walking the directory chain.
    pub fn num_data_pages(&self, pool: &mut BufferPool) -> Result<u32, StorageError> {
        if pool.num_pages(self.file) == 0 {
            return Ok(0);
        }
        let mut total = 0u32;
        let mut dir_page = PageId(0);
        loop {
            let dir = pool.fetch_page(self.file, dir_page)?;
            total += dir_count(pool.page(self.file, dir)) as u32;
            let next = dir_next(pool.page(self.file, dir));
            pool.unpin(self.file, dir, false)?;
            if next == NO_PAGE || next == PageId(0) {
                break;
            }
            dir_page = next;
        }
        Ok(total)
    }

    /// Insert one row, returning its rid.
    ///
    /// First fit over the directory pages; if none has room, a new data page is
    /// allocated and an entry added (extending the directory if needed).
    ///
    /// Errors: [`StorageError::RecordTooLong`] if the encoded record cannot fit
    /// an empty page, a codec error, or an I/O or pin error.
    pub fn insert(
        &self,
        pool: &mut BufferPool,
        values: &[Value],
    ) -> Result<RecordId, StorageError> {
        self.insert_at(pool, values, 1)
    }

    /// Insert one row, reporting `row` in a too-long-record error.
    ///
    /// The bulk loader uses this so the error names the CSV line it stopped at;
    /// a single-row insert simply reports row 1.
    ///
    /// Errors: as [`HeapFile::insert`].
    pub fn insert_at(
        &self,
        pool: &mut BufferPool,
        values: &[Value],
        row: u64,
    ) -> Result<RecordId, StorageError> {
        let record = RecordCodec::encode(&self.schema, values)?;
        let len = record.len();
        if len > slotted::MAX_RECORD {
            return Err(StorageError::RecordTooLong {
                table: self.table.clone(),
                row,
                len,
            });
        }
        let need = len + slotted::SLOT_SIZE;

        let mut dir_page = PageId(0);
        loop {
            let dir = pool.fetch_page(self.file, dir_page)?;
            let count = dir_count(pool.page(self.file, dir));

            // First fit: the first data page with room for this record.
            for i in 0..count {
                let (data_page, free) = dir_entry(pool.page(self.file, dir), i);
                if free as usize >= need {
                    let data = pool.fetch_page(self.file, data_page)?;
                    match slotted::insert(pool.page_mut(self.file, data), &record) {
                        Some(slot) => {
                            let free = slotted::free_space(pool.page(self.file, data)) as u16;
                            dir_set_entry(pool.page_mut(self.file, dir), i, data_page, free);
                            pool.unpin(self.file, data, true)?;
                            pool.unpin(self.file, dir, true)?;
                            return Ok(RecordId { page: data, slot });
                        }
                        None => {
                            // The directory's free count was optimistic (stale
                            // or corrupt). Record the page's real free space
                            // and keep looking instead of failing.
                            let free = slotted::free_space(pool.page(self.file, data)) as u16;
                            dir_set_entry(pool.page_mut(self.file, dir), i, data_page, free);
                            pool.unpin(self.file, data, true)?;
                        }
                    }
                }
            }

            // Nothing here fits: follow the chain, or grow the heap at its end.
            let next = dir_next(pool.page(self.file, dir));
            if next != NO_PAGE && next != PageId(0) {
                pool.unpin(self.file, dir, true)?;
                dir_page = next;
                continue;
            }

            // The last directory page and none of its data pages fit. Allocate a
            // data page, insert, and record it here (or on a new directory page
            // when this one is full).
            let data = pool.new_page(self.file)?;
            slotted::init(pool.page_mut(self.file, data), data);
            let Some(slot) = slotted::insert(pool.page_mut(self.file, data), &record) else {
                pool.unpin(self.file, data, true)?;
                pool.unpin(self.file, dir, true)?;
                return Err(StorageError::RecordTooLong {
                    table: self.table.clone(),
                    row,
                    len,
                });
            };
            let free = slotted::free_space(pool.page(self.file, data)) as u16;
            pool.unpin(self.file, data, true)?;

            if count < DIR_CAPACITY as u16 {
                dir_append(pool.page_mut(self.file, dir), data, free);
            } else {
                let ndpage = pool.new_page(self.file)?;
                dir_init(pool.page_mut(self.file, ndpage), ndpage);
                dir_append(pool.page_mut(self.file, ndpage), data, free);
                pool.unpin(self.file, ndpage, true)?;
                dir_set_next(pool.page_mut(self.file, dir), ndpage);
            }
            pool.unpin(self.file, dir, true)?;
            return Ok(RecordId { page: data, slot });
        }
    }

    /// Read the record at `rid`.
    ///
    /// Errors: [`StorageError::SlotOutOfRange`], [`StorageError::DeadSlot`], or
    /// I/O.
    pub fn get(&self, pool: &mut BufferPool, rid: RecordId) -> Result<Vec<u8>, StorageError> {
        let page = pool.fetch_page(self.file, rid.page)?;
        let record = slotted::get(pool.page(self.file, page), rid.slot)?.to_vec();
        pool.unpin(self.file, page, false)?;
        Ok(record)
    }

    /// Mark the record at `rid` deleted.
    ///
    /// Errors: the same as [`HeapFile::get`], plus [`StorageError::DeadSlot`] if
    /// it was already deleted.
    pub fn delete(&self, pool: &mut BufferPool, rid: RecordId) -> Result<(), StorageError> {
        let page = pool.fetch_page(self.file, rid.page)?;
        let result = slotted::delete(pool.page_mut(self.file, page), rid.slot);
        pool.unpin(self.file, page, true)?;
        result
    }

    /// Start a scan of this file. Each [`HeapCursor::next`] decodes with the
    /// schema it is given. At most two pages are pinned at once: the directory
    /// page being walked and the current data page.
    ///
    /// Errors: I/O or pin errors.
    pub fn scan(&self, pool: &mut BufferPool) -> Result<HeapCursor, StorageError> {
        let dir = if pool.num_pages(self.file) == 0 {
            None
        } else {
            Some(pool.fetch_page(self.file, PageId(0))?)
        };
        Ok(HeapCursor {
            file: self.file,
            dir,
            dir_entry: 0,
            data: None,
            data_slot: 0,
        })
    }
}

/// A borrow-free position in a heap file.
///
/// The cursor holds only where it is — which directory page and which data page
/// it is walking, and where in each — plus the file those pages live in. The
/// pool is passed into every call rather than held, so a cursor can be stored by
/// any owner that receives the pool per call: the M2 executor stores one for a
/// `seqscan`, and the M1 CLI drives one directly. A page is named by its
/// `PageId`, because that is what the buffer pool pins and unpins; entry counts,
/// the next directory page and the slot count are read back from the pinned page
/// when needed, so there is no second copy to keep in sync.
///
/// The walk pins at most two pages: the directory page being walked and the data
/// page being walked. [`HeapCursor::close`] releases them; there is no `Drop`
/// impl, because a cursor cannot unpin without the pool.
pub struct HeapCursor {
    file: FileId,
    /// The directory page being walked, and the next entry to read.
    dir: Option<PageId>,
    dir_entry: u16,
    /// The data page being walked, and the next slot to read.
    data: Option<PageId>,
    data_slot: u16,
}

impl HeapCursor {
    /// Begin at the first page of `file`, pinning the first directory page.
    ///
    /// Errors: I/O or pin errors.
    pub fn new(pool: &mut BufferPool, file: FileId) -> Result<Self, StorageError> {
        let dir = if pool.num_pages(file) == 0 {
            None
        } else {
            Some(pool.fetch_page(file, PageId(0))?)
        };
        Ok(HeapCursor {
            file,
            dir,
            dir_entry: 0,
            data: None,
            data_slot: 0,
        })
    }

    /// The next live row, decoded with `schema`. `pool` is borrowed only for the
    /// duration of the call, so the cursor stays valid in between.
    ///
    /// On any error the pages this cursor pinned are released, so a caller that
    /// stops at the error does not leak pins.
    ///
    /// Errors: I/O, pin or decode errors.
    pub fn next(
        &mut self,
        pool: &mut BufferPool,
        schema: &Schema,
    ) -> Result<Option<(RecordId, Vec<Value>)>, StorageError> {
        match self.step(pool, schema) {
            Err(err) => {
                self.close(pool);
                Err(err)
            }
            other => other,
        }
    }

    fn step(
        &mut self,
        pool: &mut BufferPool,
        schema: &Schema,
    ) -> Result<Option<(RecordId, Vec<Value>)>, StorageError> {
        loop {
            // Serve live slots from the current data page.
            if let Some(data_page) = self.data {
                let count = slotted::slot_count(pool.page(self.file, data_page));
                while self.data_slot < count {
                    let slot = self.data_slot;
                    self.data_slot += 1;
                    if slotted::is_live(pool.page(self.file, data_page), slot) {
                        let bytes = slotted::get(pool.page(self.file, data_page), slot)?;
                        let values = RecordCodec::decode(schema, bytes)?;
                        return Ok(Some((
                            RecordId {
                                page: data_page,
                                slot,
                            },
                            values,
                        )));
                    }
                }
                // Data page exhausted: unpin it and move on.
                pool.unpin(self.file, data_page, false)?;
                self.data = None;
            }

            // Fetch the next data page from the current directory page.
            if let Some(dir_page) = self.dir {
                let count = dir_count(pool.page(self.file, dir_page));
                if self.dir_entry < count {
                    let entry = self.dir_entry;
                    self.dir_entry += 1;
                    let (data_page, _free) = dir_entry(pool.page(self.file, dir_page), entry);
                    self.data = Some(pool.fetch_page(self.file, data_page)?);
                    self.data_slot = 0;
                    continue;
                }
                // Directory page exhausted: move to the next one.
                let next = dir_next(pool.page(self.file, dir_page));
                pool.unpin(self.file, dir_page, false)?;
                self.dir = None;
                if next == NO_PAGE || next == PageId(0) {
                    return Ok(None);
                }
                self.dir = Some(pool.fetch_page(self.file, next)?);
                self.dir_entry = 0;
                continue;
            }

            return Ok(None);
        }
    }

    /// Release any pinned pages. Idempotent; safe on a finished cursor.
    pub fn close(&mut self, pool: &mut BufferPool) {
        if let Some(data_page) = self.data.take() {
            let _ = pool.unpin(self.file, data_page, false);
        }
        if let Some(dir_page) = self.dir.take() {
            let _ = pool.unpin(self.file, dir_page, false);
        }
    }
}
