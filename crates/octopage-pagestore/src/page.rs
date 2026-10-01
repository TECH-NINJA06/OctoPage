use std::fmt;

use bytes::Bytes;

use crate::error::{Error, Result, format, invalid};

pub const HEADER_LEN: usize = 32;
pub const MAGIC: &[u8; 4] = b"OPG1";
pub const MAX_PAGE_ID: u32 = (1 << 24) - 1;
pub const PAGE_SIZES: [usize; 3] = [4096, 8192, 16384];
pub const FORMAT_VERSION: u32 = 1;

pub const FLAG_COMPRESSED: u8 = 1;
pub const FLAG_ENCRYPTED: u8 = 2;
pub const FLAG_OVERFLOW: u8 = 4;

const OFF_TYPE: usize = 4;
const OFF_FLAGS: usize = 5;
const OFF_CELLS: usize = 6;
const OFF_ID: usize = 8;
const OFF_FREE: usize = 12;
const OFF_EPOCH: usize = 16;

/// A 24-bit page id. Page 0 is the superblock; pages 1..=map_pages hold the free-map.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(u32);

impl PageId {
    pub const SUPERBLOCK: PageId = PageId(0);

    pub fn new(id: u32) -> Result<Self> {
        if id > MAX_PAGE_ID {
            return Err(invalid(format!("page id {id:#x} exceeds 24 bits")));
        }
        Ok(PageId(id))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    /// The three fan-out bytes: the page lives at `pages/AA/BB/CC`.
    pub(crate) fn bytes(self) -> [u8; 3] {
        [(self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8]
    }

    pub(crate) fn from_bytes(b: [u8; 3]) -> PageId {
        PageId((b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32)
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06x}", self.0)
    }
}

impl fmt::Debug for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PageId({:06x})", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Superblock = 1,
    Interior = 2,
    Leaf = 3,
    Overflow = 4,
    FreeMap = 5,
    ChangelogIndex = 6,
    /// A page whose payload belongs entirely to the layer above.
    Data = 7,
}

impl PageType {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => PageType::Superblock,
            2 => PageType::Interior,
            3 => PageType::Leaf,
            4 => PageType::Overflow,
            5 => PageType::FreeMap,
            6 => PageType::ChangelogIndex,
            7 => PageType::Data,
            _ => return None,
        })
    }

    /// Pages worth keeping in memory ahead of others: they sit on every lookup path.
    pub(crate) fn is_hot(self) -> bool {
        matches!(
            self,
            PageType::Superblock | PageType::FreeMap | PageType::Interior
        )
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// A page as stored: header plus payload, exactly the database's page size.
#[derive(Clone, PartialEq, Eq)]
pub struct Page(Bytes);

impl Page {
    /// Check a page read from the store: size, magic, type, self-reported id, and an epoch no
    /// later than the snapshot it was read through.
    pub(crate) fn parse(
        bytes: Bytes,
        page_size: usize,
        expected: PageId,
        max_epoch: u64,
    ) -> Result<Page> {
        let bad = |reason: String| Error::Corrupt {
            page: expected,
            reason,
        };
        if bytes.len() != page_size {
            return Err(bad(format!("{} bytes, expected {page_size}", bytes.len())));
        }
        if &bytes[..4] != MAGIC {
            return Err(bad("bad magic".into()));
        }
        if PageType::from_u8(bytes[OFF_TYPE]).is_none() {
            return Err(bad(format!("unknown page type {}", bytes[OFF_TYPE])));
        }
        let id = u32_at(&bytes, OFF_ID);
        if id != expected.get() {
            return Err(bad(format!("header says it is page {id:06x}")));
        }
        let epoch = u64_at(&bytes, OFF_EPOCH);
        if epoch > max_epoch {
            return Err(bad(format!(
                "written at epoch {epoch}, after the snapshot's epoch {max_epoch}"
            )));
        }
        Ok(Page(bytes))
    }

    pub fn page_type(&self) -> PageType {
        PageType::from_u8(self.0[OFF_TYPE]).expect("validated by parse")
    }

    pub fn flags(&self) -> u8 {
        self.0[OFF_FLAGS]
    }

    pub fn cell_count(&self) -> u16 {
        u16_at(&self.0, OFF_CELLS)
    }

    pub fn id(&self) -> PageId {
        PageId(u32_at(&self.0, OFF_ID))
    }

    pub fn free_offset(&self) -> u32 {
        u32_at(&self.0, OFF_FREE)
    }

    pub fn epoch(&self) -> u64 {
        u64_at(&self.0, OFF_EPOCH)
    }

    pub fn payload(&self) -> &[u8] {
        &self.0[HEADER_LEN..]
    }

    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    /// An editable copy, to modify and `put` back.
    pub fn to_buf(&self) -> PageBuf {
        PageBuf(self.0.to_vec())
    }
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Page({} {:?} epoch {} {} bytes)",
            self.id(),
            self.page_type(),
            self.epoch(),
            self.0.len()
        )
    }
}

/// A page being built or edited. The page store stamps its id and epoch on commit.
#[derive(Clone, PartialEq, Eq)]
pub struct PageBuf(Vec<u8>);

impl PageBuf {
    pub fn new(page_type: PageType, page_size: usize) -> Self {
        let mut bytes = vec![0u8; page_size];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[OFF_TYPE] = page_type as u8;
        PageBuf(bytes)
    }

    pub fn page_type(&self) -> PageType {
        PageType::from_u8(self.0[OFF_TYPE]).expect("PageBuf always has a valid type")
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn flags(&self) -> u8 {
        self.0[OFF_FLAGS]
    }

    pub fn set_flags(&mut self, flags: u8) {
        self.0[OFF_FLAGS] = flags;
    }

    pub fn cell_count(&self) -> u16 {
        u16_at(&self.0, OFF_CELLS)
    }

    pub fn set_cell_count(&mut self, n: u16) {
        self.0[OFF_CELLS..OFF_CELLS + 2].copy_from_slice(&n.to_le_bytes());
    }

    pub fn free_offset(&self) -> u32 {
        u32_at(&self.0, OFF_FREE)
    }

    pub fn set_free_offset(&mut self, offset: u32) {
        self.0[OFF_FREE..OFF_FREE + 4].copy_from_slice(&offset.to_le_bytes());
    }

    pub fn payload(&self) -> &[u8] {
        &self.0[HEADER_LEN..]
    }

    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.0[HEADER_LEN..]
    }

    /// The stored form: this page with its id and the committing epoch written into the header.
    pub(crate) fn stamp(&self, id: PageId, epoch: u64) -> Bytes {
        let mut bytes = self.0.clone();
        bytes[OFF_ID..OFF_ID + 4].copy_from_slice(&id.get().to_le_bytes());
        bytes[OFF_EPOCH..OFF_EPOCH + 8].copy_from_slice(&epoch.to_le_bytes());
        Bytes::from(bytes)
    }
}

impl fmt::Debug for PageBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PageBuf({:?} {} bytes)", self.page_type(), self.0.len())
    }
}

// ------------------------------------------------------------------------------ superblock

/// Page 0. Rewritten by every commit; its header epoch is the commit's sequence number. It is
/// always stored as it is, never compressed or encrypted.
///
/// Payload: format version (u32), page size (u32), next never-used page id (u32),
/// number of free-map pages (u32), codec flags (u32: 1 compressed, 2 encrypted; the keys are in
/// the root tree's `keys` blob).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Superblock {
    pub format: u32,
    pub page_size: u32,
    /// The high-water mark: page ids at or above it have never been allocated.
    pub next_free: u32,
    /// Free-map pages occupy ids `1..=map_pages`; data pages start after them.
    pub map_pages: u32,
    /// How the other blobs are stored ([`CODEC_COMPRESSED`], [`CODEC_ENCRYPTED`]).
    pub codec: u32,
}

pub const CODEC_COMPRESSED: u32 = 1;
pub const CODEC_ENCRYPTED: u32 = 2;

/// Free-map pages needed to cover the whole 24-bit id space at this page size.
pub(crate) fn map_pages_for(page_size: usize) -> u32 {
    let bits = ((page_size - HEADER_LEN) * 8) as u32;
    (MAX_PAGE_ID + 1).div_ceil(bits)
}

impl Superblock {
    pub(crate) fn new(page_size: usize) -> Result<Self> {
        if !PAGE_SIZES.contains(&page_size) {
            return Err(invalid(format!(
                "page size must be one of {PAGE_SIZES:?}, got {page_size}"
            )));
        }
        let map_pages = map_pages_for(page_size);
        Ok(Superblock {
            format: FORMAT_VERSION,
            page_size: page_size as u32,
            next_free: map_pages + 1,
            map_pages,
            codec: 0,
        })
    }

    pub fn first_data_page(&self) -> u32 {
        self.map_pages + 1
    }

    /// Ids 0..=map_pages: the superblock and the free-map, which the page store derives at
    /// commit time and leaves out of conflict detection.
    pub fn is_system(&self, id: PageId) -> bool {
        id.get() <= self.map_pages
    }

    pub(crate) fn bits_per_map_page(&self) -> u32 {
        (self.page_size as usize - HEADER_LEN) as u32 * 8
    }

    /// The free-map page and bit that track `id`.
    pub(crate) fn map_slot(&self, id: PageId) -> (PageId, u32) {
        let bits = self.bits_per_map_page();
        (PageId(1 + id.get() / bits), id.get() % bits)
    }

    pub(crate) fn encode(&self) -> PageBuf {
        let mut page = PageBuf::new(PageType::Superblock, self.page_size as usize);
        let p = page.payload_mut();
        p[0..4].copy_from_slice(&self.format.to_le_bytes());
        p[4..8].copy_from_slice(&self.page_size.to_le_bytes());
        p[8..12].copy_from_slice(&self.next_free.to_le_bytes());
        p[12..16].copy_from_slice(&self.map_pages.to_le_bytes());
        p[16..20].copy_from_slice(&self.codec.to_le_bytes());
        page
    }

    pub(crate) fn decode(page: &Page) -> Result<Self> {
        if page.page_type() != PageType::Superblock {
            return Err(format(format!(
                "page 0 is a {:?} page, not the superblock",
                page.page_type()
            )));
        }
        let p = page.payload();
        let sb = Superblock {
            format: u32_at(p, 0),
            page_size: u32_at(p, 4),
            next_free: u32_at(p, 8),
            map_pages: u32_at(p, 12),
            codec: u32_at(p, 16),
        };
        if sb.format != FORMAT_VERSION {
            return Err(format(format!("unsupported format version {}", sb.format)));
        }
        if sb.codec & !(CODEC_COMPRESSED | CODEC_ENCRYPTED) != 0 {
            return Err(format(format!("unknown codec flags {:#x}", sb.codec)));
        }
        if sb.page_size as usize != page.as_bytes().len()
            || !PAGE_SIZES.contains(&(sb.page_size as usize))
        {
            return Err(format(format!(
                "superblock page size {} is inconsistent",
                sb.page_size
            )));
        }
        if sb.map_pages != map_pages_for(sb.page_size as usize)
            || sb.next_free <= sb.map_pages
            || sb.next_free > MAX_PAGE_ID + 1
        {
            return Err(format("superblock free-map layout is inconsistent"));
        }
        Ok(sb)
    }
}

/// Bit `i` of a free-map page's payload: set means the page is in use.
pub(crate) fn map_bit(payload: &[u8], i: u32) -> bool {
    payload[(i / 8) as usize] & (1 << (i % 8)) != 0
}

pub(crate) fn set_map_bit(payload: &mut [u8], i: u32, used: bool) {
    let byte = &mut payload[(i / 8) as usize];
    if used {
        *byte |= 1 << (i % 8);
    } else {
        *byte &= !(1 << (i % 8));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip_and_checks() {
        let mut buf = PageBuf::new(PageType::Leaf, 4096);
        buf.set_cell_count(7);
        buf.set_free_offset(123);
        buf.set_flags(FLAG_OVERFLOW);
        buf.payload_mut()[0] = 42;
        let id = PageId::new(0x0a0b0c).unwrap();
        let page = Page::parse(buf.stamp(id, 9), 4096, id, 9).unwrap();
        assert_eq!(
            (
                page.page_type(),
                page.cell_count(),
                page.free_offset(),
                page.flags()
            ),
            (PageType::Leaf, 7, 123, FLAG_OVERFLOW)
        );
        assert_eq!((page.id(), page.epoch(), page.payload()[0]), (id, 9, 42));
        assert_eq!(page.to_buf().stamp(id, 9), *page.as_bytes());

        let other = PageId::new(5).unwrap();
        assert!(
            matches!(
                Page::parse(buf.stamp(id, 9), 4096, other, 9),
                Err(Error::Corrupt { .. })
            ),
            "wrong place in the tree"
        );
        assert!(
            Page::parse(buf.stamp(id, 10), 4096, id, 9).is_err(),
            "from the future"
        );
        assert!(
            Page::parse(buf.stamp(id, 9), 8192, id, 9).is_err(),
            "wrong size"
        );
        let mut garbage = buf.stamp(id, 9).to_vec();
        garbage[0] = b'X';
        assert!(
            Page::parse(Bytes::from(garbage), 4096, id, 9).is_err(),
            "bad magic"
        );
        assert!(PageId::new(1 << 24).is_err());
        assert_eq!(PageId::from_bytes(id.bytes()), id);
    }

    #[test]
    fn superblock_layout() {
        let sb = Superblock::new(4096).unwrap();
        assert_eq!(sb.map_pages, 517); // 16,777,216 ids / 32,512 bits per map page
        assert_eq!(sb.first_data_page(), 518);
        assert!(
            sb.is_system(PageId::new(517).unwrap()) && !sb.is_system(PageId::new(518).unwrap())
        );
        assert_eq!(
            sb.map_slot(PageId::new(32_512).unwrap()),
            (PageId::new(2).unwrap(), 0)
        );
        let page = Page::parse(
            sb.encode().stamp(PageId::SUPERBLOCK, 1),
            4096,
            PageId::SUPERBLOCK,
            1,
        )
        .unwrap();
        assert_eq!(Superblock::decode(&page).unwrap(), sb);
        assert_eq!(Superblock::new(16384).unwrap().map_pages, 129);
        assert!(Superblock::new(1000).is_err());
    }

    #[test]
    fn map_bits() {
        let mut payload = vec![0u8; 16];
        set_map_bit(&mut payload, 9, true);
        assert!(map_bit(&payload, 9) && !map_bit(&payload, 8));
        set_map_bit(&mut payload, 9, false);
        assert!(!map_bit(&payload, 9));
    }
}
