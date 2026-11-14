//! The slotted data page — §3 of the module specification, to the byte.
//!
//! The layout lives here, not on [`Page`], so the page stays a format-agnostic
//! buffer. Every function takes the raw page and reads or writes the bytes it
//! owns.
//!
//! ```text
//! offset  size  field
//!      0     4  page_id          uint32
//!      4     2  slot_count       uint16   includes dead slots
//!      6     2  free_end         uint16   lowest byte used by record data
//!      8     4  slot 0           uint16 offset, uint16 length
//!     12     4  slot 1
//!    ...
//!  free_end                      record data, newest at the lowest offset
//!    4095                         end of page
//! ```
//!
//! A slot whose offset is zero is dead. Slot numbers are never reused and never
//! renumbered, so a rid written by this module stays valid.

use super::error::StorageError;
use super::page::{Page, PageId};

/// The fixed page header: page_id (4) + slot_count (2) + free_end (2).
pub const HEADER_SIZE: usize = 8;
/// One slot directory entry: offset (2) + length (2).
pub const SLOT_SIZE: usize = 4;
/// The largest free space an empty page has. A single insert may consume at
/// most this much, because the record also needs a slot.
pub const MAX_FREE: usize = Page::SIZE - HEADER_SIZE;
/// The longest record that fits an empty page: [`MAX_FREE`] minus the slot the
/// insert needs. A longer record cannot be stored at all (§3.3, §9).
pub const MAX_RECORD: usize = MAX_FREE - SLOT_SIZE;

/// Zero the page and write its header as an empty slotted page.
pub fn init(page: &mut Page, page_id: PageId) {
    page.fill(0);
    page.write_u32(0, page_id.0);
    page.write_u16(4, 0);
    page.write_u16(6, Page::SIZE as u16);
}

/// The page id stored in the header.
pub fn page_id(page: &Page) -> PageId {
    PageId(page.read_u32(0))
}

/// The number of slots, including dead ones.
pub fn slot_count(page: &Page) -> u16 {
    page.read_u16(4)
}

/// The lowest byte offset used by record data.
pub fn free_end(page: &Page) -> u16 {
    page.read_u16(6)
}

/// Free space on the page: `free_end - (HEADER_SIZE + SLOT_SIZE * slot_count)`.
pub fn free_space(page: &Page) -> usize {
    free_end(page) as usize - (HEADER_SIZE + SLOT_SIZE * slot_count(page) as usize)
}

/// The `(offset, length)` of one slot.
pub fn slot(page: &Page, slot_no: u16) -> (u16, u16) {
    let at = HEADER_SIZE + SLOT_SIZE * slot_no as usize;
    (page.read_u16(at), page.read_u16(at + 2))
}

fn set_slot(page: &mut Page, slot_no: u16, offset: u16, len: u16) {
    let at = HEADER_SIZE + SLOT_SIZE * slot_no as usize;
    page.write_u16(at, offset);
    page.write_u16(at + 2, len);
}

/// Whether a slot currently refers to a record.
pub fn is_live(page: &Page, slot_no: u16) -> bool {
    slot(page, slot_no).0 != 0
}

/// Insert `record`, returning its new slot number, or `None` if the page has no
/// room (`NO_SPACE` in the spec).
///
/// The spec's exact algorithm: an insert needs `len + 4` free bytes because it
/// also needs a new slot, data grows down from `free_end`, and the new slot is
/// appended.
pub fn insert(page: &mut Page, record: &[u8]) -> Option<u16> {
    let len = record.len();
    if free_space(page) < len + SLOT_SIZE {
        return None;
    }
    let new_end = free_end(page) as usize - len;
    page[new_end..new_end + len].copy_from_slice(record);
    let slot_no = slot_count(page);
    set_slot(page, slot_no, new_end as u16, len as u16);
    page.write_u16(4, slot_no + 1);
    page.write_u16(6, new_end as u16);
    Some(slot_no)
}

/// Read the record in `slot_no`.
///
/// Errors: [`StorageError::SlotOutOfRange`] for a slot past the directory, or
/// [`StorageError::DeadSlot`] naming the rid for a deleted record (§9).
pub fn get(page: &Page, slot_no: u16) -> Result<&[u8], StorageError> {
    if slot_no >= slot_count(page) {
        return Err(StorageError::SlotOutOfRange {
            page: page_id(page),
            slot: slot_no,
            slot_count: slot_count(page),
        });
    }
    let (offset, len) = slot(page, slot_no);
    if offset == 0 {
        return Err(StorageError::DeadSlot {
            page: page_id(page),
            slot: slot_no,
        });
    }
    Ok(&page[offset as usize..offset as usize + len as usize])
}

/// Mark `slot_no` dead. The space is not reclaimed until compaction, which this
/// module does not implement.
///
/// Errors: [`StorageError::SlotOutOfRange`] or [`StorageError::DeadSlot`] if the
/// slot is already dead (§9).
pub fn delete(page: &mut Page, slot_no: u16) -> Result<(), StorageError> {
    if slot_no >= slot_count(page) {
        return Err(StorageError::SlotOutOfRange {
            page: page_id(page),
            slot: slot_no,
            slot_count: slot_count(page),
        });
    }
    if slot(page, slot_no).0 == 0 {
        return Err(StorageError::DeadSlot {
            page: page_id(page),
            slot: slot_no,
        });
    }
    set_slot(page, slot_no, 0, 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty(page_id: PageId) -> Box<Page> {
        let mut data = Box::new(Page::zeroed());
        init(&mut data, page_id);
        data
    }

    /// §3.2: the worked example must reproduce exactly.
    #[test]
    fn worked_example_reproduces() {
        let mut page = empty(PageId(0));

        assert_eq!(slot_count(&page), 0);
        assert_eq!(free_end(&page), 4096);
        assert_eq!(free_space(&page), 4088);

        assert_eq!(insert(&mut page, &[0u8; 13]), Some(0));
        assert_eq!(slot_count(&page), 1);
        assert_eq!(free_end(&page), 4083);
        assert_eq!(free_space(&page), 4071);
        assert_eq!(slot(&page, 0), (4083, 13));

        assert_eq!(insert(&mut page, &[0u8; 20]), Some(1));
        assert_eq!(slot_count(&page), 2);
        assert_eq!(free_end(&page), 4063);
        assert_eq!(free_space(&page), 4047);
        assert_eq!(slot(&page, 1), (4063, 20));

        assert_eq!(insert(&mut page, &[0u8; 13]), Some(2));
        assert_eq!(slot_count(&page), 3);
        assert_eq!(free_end(&page), 4050);
        assert_eq!(free_space(&page), 4030);
        assert_eq!(slot(&page, 2), (4050, 13));

        delete(&mut page, 1).unwrap();
        assert_eq!(slot_count(&page), 3);
        assert_eq!(free_end(&page), 4050);
        assert_eq!(free_space(&page), 4030);
        assert_eq!(slot(&page, 1), (0, 0));
        assert!(!is_live(&page, 1));
        assert!(is_live(&page, 0));
        assert_eq!(get(&page, 0).unwrap().len(), 13);
        assert!(matches!(
            get(&page, 1),
            Err(StorageError::DeadSlot {
                page: PageId(0),
                slot: 1
            })
        ));
    }

    #[test]
    fn full_page_refuses_an_inserting_slot() {
        // A fresh page has 4088 free bytes. A record of 4084 leaves exactly 0
        // free after the slot; one more byte does not fit.
        let mut page = empty(PageId(1));
        assert_eq!(insert(&mut page, &[0u8; 4084]), Some(0));
        assert_eq!(free_space(&page), 0);
        assert_eq!(insert(&mut page, &[0u8; 1]), None);
        assert_eq!(get(&page, 0).unwrap().len(), 4084);
    }

    #[test]
    fn deleting_twice_or_past_the_end_is_an_error() {
        let mut page = empty(PageId(7));
        insert(&mut page, &[1, 2, 3]).unwrap();
        delete(&mut page, 0).unwrap();
        match delete(&mut page, 0) {
            Err(StorageError::DeadSlot {
                page: PageId(7),
                slot: 0,
            }) => {}
            other => panic!("expected DeadSlot, got {other:?}"),
        }
        match delete(&mut page, 1) {
            Err(StorageError::SlotOutOfRange {
                page: PageId(7), ..
            }) => {}
            other => panic!("expected SlotOutOfRange, got {other:?}"),
        }
        match get(&page, 5) {
            Err(StorageError::SlotOutOfRange {
                page: PageId(7), ..
            }) => {}
            other => panic!("expected SlotOutOfRange, got {other:?}"),
        }
    }
}
