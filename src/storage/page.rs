//! The raw 4096-byte page: a byte buffer with little-endian integer accessors.
//!
//! A page makes no assumption about the layout of its bytes. Every page format
//! in this module is expressed on top of the generic accessors here:
//!
//! - the slotted data page in [`super::slotted`] (§3);
//! - the heap directory page in [`super::heap`] (§4);
//! - the catalog page in [`super::catalog`] (§6).
//!
//! Keeping the layout out of [`Page`] means the same buffer type is used for
//! every format, and each format owns the offsets and fields it cares about.

use std::fmt;
use std::str::FromStr;

/// The zero-based index of a page inside its file (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub u32);

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for PageId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u32>().map(PageId)
    }
}

/// A 4096-byte page: the one type this module and its callers pass around.
///
/// The wrapper keeps a function honest — a buffer-pool frame or a disk call
/// wants a *page*, not any `[u8; 4096]` slab — and gives the module a single
/// type to centre on. The bytes are reached by deref (indexing, slicing),
/// `AsRef`/`AsMut`, or `From<[u8; Page::SIZE]>`, and the little-endian integer
/// accessors below read and write at an explicit offset.
#[derive(Debug, Clone, Copy)]
pub struct Page([u8; Page::SIZE]);

impl Page {
    /// A page is always 4096 bytes.
    pub const SIZE: usize = 4096;

    /// A page filled with zeros: a fresh page, or a read past the end of a
    /// file.
    pub fn zeroed() -> Self {
        Page([0u8; Self::SIZE])
    }

    /// The byte at `at`.
    pub fn read_u8(&self, at: usize) -> u8 {
        self.0[at]
    }

    /// The little-endian `u16` at `at`.
    pub fn read_u16(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.0[at], self.0[at + 1]])
    }

    /// The little-endian `u32` at `at`.
    pub fn read_u32(&self, at: usize) -> u32 {
        u32::from_le_bytes([self.0[at], self.0[at + 1], self.0[at + 2], self.0[at + 3]])
    }

    /// Write `v` at `at`.
    pub fn write_u8(&mut self, at: usize, v: u8) {
        self.0[at] = v;
    }

    /// Write `v` little-endian at `at`.
    pub fn write_u16(&mut self, at: usize, v: u16) {
        self.0[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Write `v` little-endian at `at`.
    pub fn write_u32(&mut self, at: usize, v: u32) {
        self.0[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
}

impl Default for Page {
    fn default() -> Self {
        Page::zeroed()
    }
}

impl std::ops::Deref for Page {
    type Target = [u8; Page::SIZE];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Page {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AsRef<[u8]> for Page {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsMut<[u8]> for Page {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl From<[u8; Page::SIZE]> for Page {
    fn from(bytes: [u8; Page::SIZE]) -> Self {
        Page(bytes)
    }
}

impl PartialEq<[u8; Page::SIZE]> for Page {
    fn eq(&self, other: &[u8; Page::SIZE]) -> bool {
        &self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip_little_endian() {
        let mut page = Page::zeroed();
        page.write_u8(0, 0xAB);
        page.write_u16(1, 0x1234);
        page.write_u32(3, 0x89AB_CDEF);
        assert_eq!(page.read_u8(0), 0xAB);
        assert_eq!(page.read_u16(1), 0x1234);
        assert_eq!(page.read_u32(3), 0x89AB_CDEF);
        // Little-endian on the wire: the low byte lands first.
        assert_eq!(page[1], 0x34);
        assert_eq!(page[2], 0x12);
    }

    #[test]
    fn page_ids_parse_and_print() {
        assert_eq!("42".parse::<PageId>().unwrap(), PageId(42));
        assert_eq!(PageId(42).to_string(), "42");
        assert!("x".parse::<PageId>().is_err());
    }
}
