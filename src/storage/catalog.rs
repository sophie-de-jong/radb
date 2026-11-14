//! The catalog: `catalog.db`, read and written through the buffer pool.
//!
//! §6 leaves the on-page format to the implementer but requires it to store, per
//! table, the name, the ordered columns with type and length, the data file
//! name, the number of pages and the number of live tuples. Module 4 adds its
//! statistics to the same structure.
//!
//! ## Page format
//!
//! The catalog is a chain of 4096-byte pages. Page 0 starts with a 12-byte
//! header; the serialized blob starts at offset 12 and continues on pages
//! 1..`page_count`.
//!
//! ```text
//! offset  size  field
//!      0     4  magic  = 0x5241_4442 ("RADB")
//!      4     2  version = 3
//!      6     2  page_count    total catalog pages, including page 0
//!      8     4  total_len     byte length of the blob
//!     12   ...  blob bytes, continuing on the following pages
//! ```
//!
//! A blob that needs more than one page is written across the chain. If the
//! catalog shrinks, the old trailing pages are simply ignored; `page_count`
//! says how many pages belong to the current blob.
//!
//! ## Blob format
//!
//! The blob is a bincode encoding of `Vec<TableMeta>` with fixed-width
//! little-endian integers and trailing-byte rejection. The serialized schema is
//! the field order below: `TableMeta` (name, file name, page count, tuple
//! count, schema, optional statistics), where `Schema` is a `Vec<Column>` and
//! `Column` is a name plus a `ColumnType`. This is our catalog format; the
//! record format of the heap pages is prescribed separately and is untouched.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::buffer::{BufferPool, FileId};
use super::codec::BinaryCodec;
use super::error::StorageError;
use super::page::{Page, PageId};
use super::stats::Stats;
use crate::schema::Schema;

/// The catalog page magic, `"RADB"`.
pub const CATALOG_MAGIC: u32 = 0x52_41_44_42;
/// The catalog format version.
pub const CATALOG_VERSION: u16 = 3;
/// Catalog page header size: magic (4) + version (2) + page_count (2) +
/// total_len (4).
pub const CATALOG_HEADER: usize = 12;
/// Blob bytes that fit on page 0 after the header.
const CATALOG_ROOM: usize = Page::SIZE - CATALOG_HEADER;

/// Everything the catalog stores about one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableMeta {
    /// The table name, as used on the command line.
    pub name: String,
    /// The data file name, for example `R.db`.
    pub file_name: PathBuf,
    /// The number of pages in the data file, as known at the last write.
    pub num_pages: u32,
    /// The number of live tuples.
    pub num_tuples: u64,
    /// The ordered columns.
    pub schema: Schema,
    /// Module 4 statistics, if `ANALYZE` has run for this table.
    #[serde(default)]
    pub stats: Option<Stats>,
}

/// Reject table names that could escape the database directory or clash with
/// the catalog file.
///
/// The SQL identifier rule and the storage file naming both permit exactly
/// ASCII letters, digits and `_`, and `catalog` is reserved for `catalog.db`.
pub fn validate_table_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty() || name.eq_ignore_ascii_case("catalog") {
        return Err(StorageError::BadSchema {
            detail: format!("invalid table name '{name}'"),
        });
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(StorageError::BadSchema {
            detail: format!("table name '{name}' may only contain letters, digits and '_'"),
        });
    }
    Ok(())
}

impl TableMeta {
    /// A metadata record for a brand-new, empty table.
    pub fn new(name: impl Into<String>, schema: Schema) -> Self {
        let name = name.into();
        let mut file_name = PathBuf::from(&name);
        file_name.set_extension("db");
        TableMeta {
            file_name,
            name,
            num_pages: 0,
            num_tuples: 0,
            schema,
            stats: None,
        }
    }
}

/// The open catalog: which pages it occupies, and its tables in creation order.
pub struct Catalog {
    /// The buffer-pool file id of `catalog.db`.
    pub file: FileId,
    tables: Vec<TableMeta>,
}

impl Catalog {
    /// Open (creating if needed) the catalog and load its tables.
    ///
    /// A missing or zero-page file is an empty catalog.
    ///
    /// Errors: I/O, or [`StorageError::Corrupt`] for a bad magic/version or a
    /// malformed blob.
    pub fn open(pool: &mut BufferPool, path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let file = pool.open_file(path)?;
        if pool.num_pages(file) == 0 {
            return Ok(Catalog {
                file,
                tables: Vec::new(),
            });
        }
        let blob = read_blob(pool, file)?;
        let tables = BinaryCodec::decode(&blob)?;
        Ok(Catalog { file, tables })
    }

    /// The tables, in creation order.
    pub fn tables(&self) -> &[TableMeta] {
        &self.tables
    }

    /// Look a table up by name.
    pub fn get(&self, name: &str) -> Option<&TableMeta> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Look a table up by name, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut TableMeta> {
        self.tables.iter_mut().find(|t| t.name == name)
    }

    /// Look a table up by name, or fail with [`StorageError::UnknownTable`].
    pub fn require(&self, name: &str) -> Result<&TableMeta, StorageError> {
        self.get(name).ok_or_else(|| StorageError::UnknownTable {
            name: name.to_string(),
        })
    }

    /// Look a table up by name, mutably, or fail with [`StorageError::UnknownTable`].
    pub fn require_mut(&mut self, name: &str) -> Result<&mut TableMeta, StorageError> {
        self.get_mut(name)
            .ok_or_else(|| StorageError::UnknownTable {
                name: name.to_string(),
            })
    }

    /// Add a table. The caller is responsible for its data file.
    ///
    /// Errors: [`StorageError::DuplicateTable`].
    pub fn insert(&mut self, meta: TableMeta) -> Result<(), StorageError> {
        if self.get(&meta.name).is_some() {
            return Err(StorageError::DuplicateTable { name: meta.name });
        }
        self.tables.push(meta);
        Ok(())
    }

    /// Serialize every table and write the catalog pages through the pool.
    ///
    /// Errors: [`StorageError::Corrupt`] if the blob needs more pages than the
    /// header can address, I/O or pin errors.
    pub fn save(&self, pool: &mut BufferPool) -> Result<(), StorageError> {
        let blob = BinaryCodec::encode(&self.tables)?;
        let page_count = catalog_pages(blob.len())?;
        write_catalog(pool, self.file, &blob, page_count)
    }
}

/// The number of catalog pages needed for `total_len` blob bytes.
fn catalog_pages(total_len: usize) -> Result<u16, StorageError> {
    let first = total_len.min(CATALOG_ROOM);
    let rest = total_len - first;
    let pages = 1 + rest.div_ceil(Page::SIZE);
    u16::try_from(pages).map_err(|_| StorageError::Corrupt {
        detail: format!(
            "catalog needs {pages} pages; the header can address at most {}",
            u16::MAX
        ),
    })
}

/// Write the header and every blob page.
///
/// Continuation pages are written first, then page 0, so a failure leaves the
/// previous header (and therefore the previous catalog) intact.
fn write_catalog(
    pool: &mut BufferPool,
    file: FileId,
    blob: &[u8],
    page_count: u16,
) -> Result<(), StorageError> {
    let total_len = u32::try_from(blob.len()).map_err(|_| StorageError::Corrupt {
        detail: "catalog blob is larger than 4 GiB".to_string(),
    })?;

    let mut offset = CATALOG_ROOM;
    for index in 1..page_count {
        let pinned = pool.fetch_page(file, PageId(index as u32))?;
        {
            let page = pool.page_mut(file, pinned);
            page.fill(0);
            let take = blob.len().saturating_sub(offset).min(Page::SIZE);
            if take > 0 {
                page[..take].copy_from_slice(&blob[offset..offset + take]);
            }
            offset += take;
        }
        pool.unpin(file, pinned, true)?;
    }

    let pinned = pool.fetch_page(file, PageId(0))?;
    {
        let page = pool.page_mut(file, pinned);
        page.fill(0);
        page.write_u32(0, CATALOG_MAGIC);
        page.write_u16(4, CATALOG_VERSION);
        page.write_u16(6, page_count);
        page.write_u32(8, total_len);
        let take = blob.len().min(CATALOG_ROOM);
        page[CATALOG_HEADER..CATALOG_HEADER + take].copy_from_slice(&blob[..take]);
    }
    pool.unpin(file, pinned, true)
}

/// Read the blob from the catalog page chain.
fn read_blob(pool: &mut BufferPool, file: FileId) -> Result<Vec<u8>, StorageError> {
    let pinned = pool.fetch_page(file, PageId(0))?;
    let (page_count, total_len, first) = {
        let page = pool.page(file, pinned);
        let (page_count, total_len) = read_catalog_header(page)?;
        let take = total_len.min(CATALOG_ROOM);
        (
            page_count,
            total_len,
            page[CATALOG_HEADER..CATALOG_HEADER + take].to_vec(),
        )
    };
    pool.unpin(file, pinned, false)?;

    let available = pool.num_pages(file) as usize;
    if available < page_count as usize {
        return Err(StorageError::Corrupt {
            detail: format!("catalog claims {page_count} pages but the file has {available}"),
        });
    }

    let mut blob = Vec::with_capacity(total_len);
    blob.extend_from_slice(&first);
    for index in 1..page_count {
        let pinned = pool.fetch_page(file, PageId(index as u32))?;
        {
            let page = pool.page(file, pinned);
            let take = (total_len - blob.len()).min(Page::SIZE);
            blob.extend_from_slice(&page[..take]);
        }
        pool.unpin(file, pinned, false)?;
    }
    Ok(blob)
}

/// Validate page 0's catalog header and return `(page_count, total_len)`.
fn read_catalog_header(page: &Page) -> Result<(u16, usize), StorageError> {
    let magic = page.read_u32(0);
    if magic != CATALOG_MAGIC {
        return Err(StorageError::Corrupt {
            detail: format!(
                "catalog page 0 has magic {magic:#010x}, expected {CATALOG_MAGIC:#010x}"
            ),
        });
    }
    let version = page.read_u16(4);
    if version != CATALOG_VERSION {
        let detail = if version < CATALOG_VERSION {
            format!(
                "catalog version {version} is no longer supported; recreate the database (expected {CATALOG_VERSION})"
            )
        } else {
            format!("catalog version {version}, expected {CATALOG_VERSION}")
        };
        return Err(StorageError::Corrupt { detail });
    }
    let page_count = page.read_u16(6);
    if page_count == 0 {
        return Err(StorageError::Corrupt {
            detail: "catalog page count is zero".to_string(),
        });
    }
    let total_len = page.read_u32(8) as usize;
    let capacity = page_count as usize * Page::SIZE - CATALOG_HEADER;
    if total_len > capacity {
        return Err(StorageError::Corrupt {
            detail: format!(
                "catalog blob length {total_len} exceeds {capacity} bytes in {page_count} pages"
            ),
        });
    }
    Ok((page_count, total_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stats::{Bucket, ColumnStats, TableStats};

    fn sample() -> TableMeta {
        let schema = "a:INT,b:VARCHAR(20),c:INT".parse::<Schema>().unwrap();
        let mut meta = TableMeta::new("R", schema);
        meta.num_pages = 3;
        meta.num_tuples = 7;
        meta.stats = Some(Stats {
            table: TableStats {
                n_tuples: 7,
                n_pages: 3,
                width: 13,
            },
            columns: vec![ColumnStats {
                ndv: 4,
                min: crate::Value::Int(1),
                max: crate::Value::Int(4),
                buckets: vec![Bucket {
                    min: crate::Value::Int(1),
                    max: crate::Value::Int(4),
                    count: 7,
                    ndv: 4,
                }],
            }],
        });
        meta
    }

    #[test]
    fn blob_round_trips() {
        let tables = vec![sample()];
        let blob = BinaryCodec::encode(&tables).unwrap();
        let back: Vec<TableMeta> = BinaryCodec::decode(&blob).unwrap();
        assert_eq!(back, tables);
        assert_eq!(back[0].file_name.as_path(), "R.db");
    }

    #[test]
    fn empty_blob_round_trips() {
        let tables: Vec<TableMeta> = Vec::new();
        let blob = BinaryCodec::encode(&tables).unwrap();
        let back: Vec<TableMeta> = BinaryCodec::decode(&blob).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn rejects_version_one() {
        let mut page = Page::zeroed();
        page.write_u32(0, CATALOG_MAGIC);
        page.write_u16(4, 1);
        page.write_u16(6, 1);
        page.write_u32(8, 8);
        let error = read_catalog_header(&page).unwrap_err();
        assert!(error.to_string().contains("version 1"), "{error}");
    }
}
