//! Runs and partitions: tuples spilled to a temporary file and read back.
//!
//! A *run* is a sequence of tuples in a temporary file, written one page at a
//! time. Sorts write runs (pass 0 and every merge pass), the sort-merge join
//! spills a large key run, the hash join keeps one partition per number, and the
//! set operators spill their inputs. All of them share this format:
//!
//! ```text
//! a page:  count u16, then `count` records of [len u16][bytes]
//! ```
//!
//! Each record is the tuple encoded exactly as §2 of the storage engine encodes
//! a record — 4-byte little-endian `INT`, `u16`-length-prefixed UTF-8
//! `VARCHAR` — so a spilled tuple is byte-for-byte the tuple stored on disk.
//! The group-by's `AVG` output is one more such `INT`, scaled by 10⁴ (§2.6).
//!
//! [`RunReader`] reads a run back one page at a time and can
//! [`rewind`](RunReader::rewind) to read it again, which is what the
//! sort-merge join's inner-run rewind needs.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::ColumnType;
use crate::storage::{Disk, Page, PageId};
use crate::value::{Row, Value};

use super::error::ExecError;
use crate::exec::{Context, Counters};
use crate::storage::TempFileDisk;

/// Encode `values` as §2 encodes a record.
///
/// Errors: [`ExecError::Arity`] for the wrong number of values, or
/// [`ExecError::Type`] for a value that does not fit its column's type.
pub fn encode_values(types: &[ColumnType], values: &[Value]) -> Result<Vec<u8>, ExecError> {
    if values.len() != types.len() {
        return Err(ExecError::Arity {
            detail: format!(
                "tuple has {} values, schema has {}",
                values.len(),
                types.len()
            ),
        });
    }
    let mut out = Vec::new();
    for (value, ty) in values.iter().zip(types.iter()) {
        match (value, ty) {
            (Value::Int(i), ColumnType::Int) => out.extend_from_slice(&i.to_le_bytes()),
            (Value::Varchar(s), ColumnType::Varchar(n)) => {
                let bytes = s.as_bytes();
                if bytes.len() > *n as usize {
                    return Err(ExecError::Type {
                        detail: format!(
                            "a string of {} bytes does not fit VARCHAR({n})",
                            bytes.len()
                        ),
                    });
                }
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            (value, ty) => {
                return Err(ExecError::Type {
                    detail: format!("value {value} does not fit column type {ty:?}"),
                });
            }
        }
    }
    Ok(out)
}

/// A conservative estimate of one in-memory row's size in bytes: the encoded
/// record plus the Vec/Arc/`Value` bookkeeping that does not appear on disk.
/// This is what bounds hash tables, outer blocks and in-memory partitions
/// against the B-frame account (DESIGN_M2.md, "how a frame is measured").
pub(crate) fn row_ram_size(row: &Row, types: &[ColumnType]) -> Result<usize, ExecError> {
    Ok(encode_values(types, row)?.len() + 64)
}

/// Decode a record encoded by [`encode_values`].
///
/// Errors: [`ExecError::Type`] if the bytes are truncated or malformed.
pub fn decode_values(types: &[ColumnType], bytes: &[u8]) -> Result<Vec<Value>, ExecError> {
    let mut out = Vec::with_capacity(types.len());
    let mut at = 0usize;
    for ty in types {
        match ty {
            ColumnType::Int => {
                let end = at + 4;
                let slice = bytes.get(at..end).ok_or_else(short_record)?;
                out.push(Value::Int(i32::from_le_bytes([
                    slice[0], slice[1], slice[2], slice[3],
                ])));
                at = end;
            }
            ColumnType::Varchar(_) => {
                let prefix = bytes.get(at..at + 2).ok_or_else(short_record)?;
                let len = u16::from_le_bytes([prefix[0], prefix[1]]) as usize;
                let start = at + 2;
                let end = start + len;
                let slice = bytes.get(start..end).ok_or_else(short_record)?;
                let text = std::str::from_utf8(slice).map_err(|_| ExecError::Type {
                    detail: "a spilled string is not UTF-8".to_string(),
                })?;
                out.push(Value::Varchar(text.to_string()));
                at = end;
            }
        }
    }
    if at != bytes.len() {
        return Err(ExecError::Type {
            detail: "a spilled record has trailing bytes".to_string(),
        });
    }
    Ok(out)
}

/// The error for a truncated spilled record.
fn short_record() -> ExecError {
    ExecError::Type {
        detail: "a spilled record is truncated".to_string(),
    }
}

/// Writes tuples to a temporary file, one page at a time.
pub struct RunWriter {
    file: TempFileDisk,
    types: Vec<ColumnType>,
    page: Page,
    /// Bytes used in the current page, including its 2-byte count.
    used: usize,
    count: u16,
    pages: u32,
    writes: u64,
}

impl RunWriter {
    /// A new run writer over a fresh temporary file in `dir`.
    pub fn new(dir: &std::path::Path, types: Vec<ColumnType>) -> Result<Self, ExecError> {
        let mut page = Page::zeroed();
        page[0] = 0;
        page[1] = 0;
        let file = TempFileDisk::create(dir).map_err(ExecError::Io)?;
        Ok(RunWriter {
            file,
            types,
            page,
            used: 2,
            count: 0,
            pages: 0,
            writes: 0,
        })
    }

    /// Append `row` to the run.
    pub fn push(&mut self, row: &Row) -> Result<(), ExecError> {
        let bytes = encode_values(&self.types, row)?;
        let needed = 2 + bytes.len();
        if self.used + needed > Page::SIZE {
            self.flush_page()?;
        }
        if self.used + needed > Page::SIZE {
            return Err(ExecError::Type {
                detail: format!("a tuple of {needed} bytes cannot be spilled in one page"),
            });
        }
        let at = self.used;
        self.page[at..at + 2].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
        self.page[at + 2..at + 2 + bytes.len()].copy_from_slice(&bytes);
        self.used += needed;
        self.count += 1;
        Ok(())
    }

    /// The number of pages written so far.
    pub fn pages(&self) -> u32 {
        self.pages
    }

    /// Disk writes this writer performed.
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Flush the last page and return a reader over the whole run, and the
    /// number of disk writes performed.
    pub fn finish(mut self) -> Result<(RunReader, u64), ExecError> {
        self.flush_page()?;
        let RunWriter {
            file,
            types,
            pages,
            writes,
            ..
        } = self;
        Ok((RunReader::new(file, types, pages), writes))
    }

    /// Write the current page, if it holds anything, and start a new one.
    fn flush_page(&mut self) -> Result<(), ExecError> {
        if self.count == 0 {
            return Ok(());
        }
        self.page[0..2].copy_from_slice(&self.count.to_le_bytes());
        let id = self.file.allocate_page();
        self.file.write_page(id, &self.page)?;
        self.writes += 1;
        self.pages += 1;
        self.page = Page::zeroed();
        self.used = 2;
        self.count = 0;
        Ok(())
    }
}

/// Reads a run back, one page at a time, and can rewind.
pub struct RunReader {
    disk: TempFileDisk,
    types: Vec<ColumnType>,
    total_pages: u32,
    next_page: u32,
    current: Option<Page>,
    offset: usize,
    remaining: u16,
    reads: u64,
}

impl RunReader {
    /// A reader over a run of `total_pages` pages.
    fn new(disk: TempFileDisk, types: Vec<ColumnType>, total_pages: u32) -> Self {
        RunReader {
            disk,
            types,
            total_pages,
            next_page: 0,
            current: None,
            offset: 0,
            remaining: 0,
            reads: 0,
        }
    }

    /// The next tuple, or `None` at end of run.
    #[allow(clippy::should_implement_trait)] // it yields a `Result`, not an item
    pub fn next(&mut self) -> Result<Option<Row>, ExecError> {
        loop {
            if self.remaining == 0 {
                self.current = None;
                if self.next_page >= self.total_pages {
                    return Ok(None);
                }
                let page = self.disk.read_page(PageId(self.next_page))?;
                self.reads += 1;
                self.next_page += 1;
                let count = u16::from_le_bytes([page[0], page[1]]);
                self.offset = 2;
                self.remaining = count;
                self.current = Some(page);
                continue;
            }
            let (len, start) = {
                let page = self.current.as_ref().expect("a page is loaded");
                let len = u16::from_le_bytes([page[self.offset], page[self.offset + 1]]) as usize;
                (len, self.offset + 2)
            };
            let bytes =
                self.current.as_ref().expect("a page is loaded")[start..start + len].to_vec();
            let values = decode_values(&self.types, &bytes)?;
            self.offset = start + len;
            self.remaining -= 1;
            return Ok(Some(Row::from(values)));
        }
    }

    /// Start reading from the beginning again.
    pub fn rewind(&mut self) {
        self.next_page = 0;
        self.current = None;
        self.offset = 0;
        self.remaining = 0;
    }

    /// The number of pages in the run.
    pub fn pages(&self) -> u32 {
        self.total_pages
    }

    /// Disk reads this reader performed.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    /// Read this run into memory up to `capacity` bytes, returning the rows read
    /// and whether the run was larger than that. A `distinct` or `group_by` in
    /// hash mode uses this to decide whether a partition fits its in-memory table
    /// or must be sorted instead.
    pub(crate) fn read_bounded(
        &mut self,
        types: &[ColumnType],
        capacity: usize,
    ) -> Result<(Vec<Row>, bool), ExecError> {
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        while let Some(row) = self.next()? {
            let size = row_ram_size(&row, types)?;
            if !rows.is_empty() && bytes + size > capacity {
                return Ok((rows, true));
            }
            bytes += size;
            rows.push(row);
        }
        Ok((rows, false))
    }
}

/// Spills rows into `partitions` runs on the hash of `keys`.
///
/// The hash-based `distinct` and `group_by` spill their input this way before
/// collapsing one partition at a time, and the hash join spills its build and
/// probe sides this way before probing partition by partition.
pub(crate) struct Partitioner {
    /// One writer per partition.
    writers: Vec<RunWriter>,
    /// The columns hashed to pick a partition.
    keys: Vec<usize>,
}

impl Partitioner {
    /// A partitioner making `partitions` runs of columns `types`, on the hash of
    /// `keys`.
    pub(crate) fn new(
        ctx: &Context,
        partitions: usize,
        keys: Vec<usize>,
        types: Vec<ColumnType>,
    ) -> Result<Self, ExecError> {
        let writers = (0..partitions)
            .map(|_| RunWriter::new(ctx.temp_dir(), types.clone()))
            .collect::<Result<_, _>>()?;
        Ok(Partitioner { writers, keys })
    }

    /// Spill one row into the run its key hashes to.
    pub(crate) fn push(&mut self, row: &Row) -> Result<(), ExecError> {
        let key = row.key(&self.keys)?;
        // A fixed-seed hasher, so partition assignment is deterministic across
        // runs — unlike a `HashMap`'s randomly-seeded `RandomState`, which would
        // make the reported reads and writes vary.
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let h = (hasher.finish() % self.writers.len() as u64) as usize;
        self.writers[h].push(row)
    }

    /// Finish the runs, counting their writes on `counters`, and return the
    /// readers, one per partition.
    pub(crate) fn finish(self, counters: &mut Counters) -> Result<Vec<RunReader>, ExecError> {
        let mut readers = Vec::with_capacity(self.writers.len());
        for writer in self.writers {
            let (reader, writes) = writer.finish()?;
            counters.page_writes += writes;
            readers.push(reader);
        }
        Ok(readers)
    }
}
