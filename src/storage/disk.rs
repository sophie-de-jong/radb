//! The disk manager: fixed 4096-byte pages addressed by page id.
//!
//! One file holds one table, plus `catalog.db`. A page id is the zero-based
//! index of a page inside its file. Every read and write goes through this
//! layer, and the buffer pool counts them.
//!
//! `FileDisk` is the real thing. `NullDisk` is a synthetic, always-zero file
//! used by the `trace` command, which only grades the replacement policy: the
//! bytes do not matter, and a synthetic disk keeps the command free of side
//! effects. `TempFileDisk` wraps a `FileDisk` and deletes the file when it is
//! dropped, for the blocking operators' spill files.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::page::{Page, PageId};

/// A page source. Reads and writes are always whole pages.
pub trait Disk {
    /// Read one full page. Reading past the end returns a zero page, so a
    /// sparse or freshly allocated page reads as zeros.
    fn read_page(&mut self, page_id: PageId) -> io::Result<Page>;
    /// Write one full page, extending the file if needed.
    fn write_page(&mut self, page_id: PageId, data: &Page) -> io::Result<()>;
    /// Reserve a new page id at the end of the file. The page reads as zeros
    /// until something is written to it.
    fn allocate_page(&mut self) -> PageId;
    /// The number of pages the file currently holds.
    fn num_pages(&self) -> u32;
}

/// A process-wide counter, so temp files never collide.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A temporary [`FileDisk`] that deletes its file when it is dropped.
///
/// The file's path lives on the wrapped [`FileDisk`], so this type only adds
/// the "delete on drop" behaviour.
pub struct TempFileDisk {
    disk: FileDisk,
}

impl TempFileDisk {
    /// Create an empty temporary file inside `dir`.
    ///
    /// Errors: an I/O failure creating the file.
    pub fn create(dir: impl AsRef<Path>) -> io::Result<Self> {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = dir
            .as_ref()
            .join(format!("radb-m2-{}-{n}.tmp", std::process::id()));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(TempFileDisk {
            disk: FileDisk {
                path: path.to_path_buf(),
                file,
                num_pages: 0,
            },
        })
    }
}

impl Disk for TempFileDisk {
    fn read_page(&mut self, id: PageId) -> io::Result<Page> {
        self.disk.read_page(id)
    }

    fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        self.disk.write_page(id, page)
    }

    fn allocate_page(&mut self) -> PageId {
        self.disk.allocate_page()
    }

    fn num_pages(&self) -> u32 {
        self.disk.num_pages()
    }
}

impl Drop for TempFileDisk {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.disk.path());
    }
}

/// A disk backed by a real file.
pub struct FileDisk {
    path: PathBuf,
    file: File,
    num_pages: u32,
}

impl FileDisk {
    /// Open (creating if needed) a page file.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        if len % Page::SIZE as u64 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a whole number of pages", path.display()),
            ));
        }
        Ok(FileDisk {
            path: path.to_path_buf(),
            file,
            num_pages: (len / Page::SIZE as u64) as u32,
        })
    }

    /// The path this file was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Disk for FileDisk {
    fn read_page(&mut self, page_id: PageId) -> io::Result<Page> {
        let mut data = Page::zeroed();
        if page_id.0 >= self.num_pages {
            // Past the end: reads as zeros, exactly like a hole.
            return Ok(data);
        }
        self.file
            .seek(SeekFrom::Start(page_id.0 as u64 * Page::SIZE as u64))?;
        self.file.read_exact(data.as_mut())?;
        Ok(data)
    }

    fn write_page(&mut self, page_id: PageId, data: &Page) -> io::Result<()> {
        self.file
            .seek(SeekFrom::Start(page_id.0 as u64 * Page::SIZE as u64))?;
        self.file.write_all(&data[..])?;
        self.num_pages = self.num_pages.max(page_id.0 + 1);
        Ok(())
    }

    fn allocate_page(&mut self) -> PageId {
        let id = self.num_pages;
        self.num_pages += 1;
        PageId(id)
    }

    fn num_pages(&self) -> u32 {
        self.num_pages
    }
}

/// An always-zero, throwaway disk used by `trace`.
#[derive(Default)]
pub struct NullDisk {
    num_pages: u32,
}

impl Disk for NullDisk {
    fn read_page(&mut self, _page_id: PageId) -> io::Result<Page> {
        Ok(Page::zeroed())
    }

    fn write_page(&mut self, page_id: PageId, _data: &Page) -> io::Result<()> {
        self.num_pages = self.num_pages.max(page_id.0 + 1);
        Ok(())
    }

    fn allocate_page(&mut self) -> PageId {
        let id = self.num_pages;
        self.num_pages += 1;
        PageId(id)
    }

    fn num_pages(&self) -> u32 {
        self.num_pages
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("radb_disk_test_{}_{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn write_read_and_allocate_round_trip() {
        let path = temp_path("rw");
        let mut disk = FileDisk::open(&path).unwrap();
        assert_eq!(disk.num_pages(), 0);

        let page = disk.allocate_page();
        assert_eq!(page, PageId(0));
        let mut data = Page::zeroed();
        data[0] = 42;
        disk.write_page(page, &data).unwrap();
        assert_eq!(disk.num_pages(), 1);

        // Past the end reads as zeros.
        let beyond = disk.read_page(PageId(9)).unwrap();
        assert_eq!(beyond, [0u8; Page::SIZE]);

        let back = disk.read_page(page).unwrap();
        assert_eq!(back[0], 42);
        let _ = std::fs::remove_file(&path);
    }
}
