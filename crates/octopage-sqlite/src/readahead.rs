use std::collections::{HashMap, VecDeque};

/// Interior pages remembered per transaction.
const TRACKED: usize = 16;
const FIRST_BATCH: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Debug)]
struct Cursor {
    /// Index of the child SQLite read last (-1 or `len` for a scan that has just entered).
    last: isize,
    /// Children already read or requested: indexes `from..to`.
    from: usize,
    to: usize,
    /// The size of the last batch requested (0 for none), and of the next one. A new batch goes
    /// out once half of the last one is left to read.
    last_batch: usize,
    next_batch: usize,
}

impl Cursor {
    fn at(index: usize) -> Self {
        Cursor {
            last: index as isize,
            from: index,
            to: index + 1,
            last_batch: 0,
            next_batch: FIRST_BATCH,
        }
    }
}

#[derive(Default)]
pub(crate) struct ReadAhead {
    /// Interior pages read recently: page number and children in key order.
    parents: VecDeque<(u32, Vec<u32>)>,
    cursors: HashMap<u32, Cursor>,
}

fn be16(page: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(page.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn be32(page: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(page.get(at..at + 4)?.try_into().ok()?))
}

/// The children of a B-tree interior page, in key order, or `None` for any other page.
pub(crate) fn interior_children(n: u32, page: &[u8]) -> Option<Vec<u32>> {
    let h = if n == 1 { 100 } else { 0 }; // page 1 starts with the database header
    let kind = *page.get(h)?;
    if kind != 0x02 && kind != 0x05 {
        return None; // not an index (2) or table (5) interior page
    }
    let cells = be16(page, h + 3)?;
    let mut children = Vec::with_capacity(cells + 1);
    for k in 0..cells {
        let at = be16(page, h + 12 + 2 * k)?;
        if at + 4 > page.len().saturating_sub(crate::RESERVED) {
            return None;
        }
        children.push(be32(page, at)?);
    }
    children.push(be32(page, h + 8)?); // the right-most child
    children.iter().all(|&c| c > 1).then_some(children)
}

impl ReadAhead {
    /// SQLite read page `n`: returns the pages to fetch ahead, if any.
    pub(crate) fn on_read(&mut self, n: u32, page: &[u8], max_batch: usize) -> Vec<u32> {
        let mut wanted = Vec::new();
        let mut scanning = None;
        if let Some((parent, index)) = self.locate(n) {
            let children = &self.parents.iter().find(|(p, _)| *p == parent).unwrap().1;
            let cursor = self
                .cursors
                .entry(parent)
                .or_insert_with(|| Cursor::at(index));
            let i = index as isize;
            let direction = if i == cursor.last + 1 {
                Some(Direction::Forward)
            } else if i == cursor.last - 1 {
                Some(Direction::Backward)
            } else {
                None
            };
            let batch = match direction {
                Some(Direction::Forward) => {
                    let left = cursor.to.saturating_sub(index + 1);
                    let start = cursor.to.max(index + 1);
                    let end = (start + cursor.next_batch).min(children.len());
                    (left * 2 <= cursor.last_batch && start < end).then(|| {
                        cursor.to = end;
                        start..end
                    })
                }
                Some(Direction::Backward) => {
                    let left = index.saturating_sub(cursor.from);
                    let end = cursor.from.min(index);
                    let start = end.saturating_sub(cursor.next_batch);
                    (left * 2 <= cursor.last_batch && start < end).then(|| {
                        cursor.from = start;
                        start..end
                    })
                }
                None => {
                    *cursor = Cursor::at(index); // random access: start over from here
                    None
                }
            };
            if let Some(range) = batch {
                cursor.last_batch = range.len();
                cursor.next_batch = (cursor.next_batch * 2).min(max_batch.max(1));
                wanted.extend_from_slice(&children[range]);
            }
            if direction.is_some() {
                cursor.last = i;
                scanning = direction.map(|d| (d, cursor.next_batch));
            }
        }
        if let Some(children) = interior_children(n, page) {
            // A scan that reaches a new interior page will read its children from one end.
            if let Some((direction, batch)) = scanning {
                let len = children.len();
                let take = batch.min(len);
                let (last, from, to) = match direction {
                    Direction::Forward => (-1, 0, take),
                    Direction::Backward => (len as isize, len - take, len),
                };
                wanted.extend_from_slice(&children[from..to]);
                let cursor = Cursor {
                    last,
                    from,
                    to,
                    last_batch: take,
                    next_batch: (batch * 2).min(max_batch.max(1)),
                };
                self.cursors.insert(n, cursor);
            } else {
                self.cursors.remove(&n);
            }
            self.remember(n, children);
        }
        wanted
    }

    /// The tracked interior page that has `n` as a child, and its index there.
    fn locate(&self, n: u32) -> Option<(u32, usize)> {
        self.parents
            .iter()
            .rev()
            .find_map(|(p, children)| children.iter().position(|&c| c == n).map(|i| (*p, i)))
    }

    fn remember(&mut self, n: u32, children: Vec<u32>) {
        self.parents.retain(|(p, _)| *p != n);
        if self.parents.len() == TRACKED
            && let Some((old, _)) = self.parents.pop_front()
        {
            self.cursors.remove(&old);
        }
        self.parents.push_back((n, children));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table interior page (type 5) at page `n` with the given children.
    fn interior(n: u32, children: &[u32]) -> Vec<u8> {
        let mut page = vec![0u8; crate::PAGE_SIZE];
        let h = if n == 1 { 100 } else { 0 };
        let cells = children.len() - 1;
        page[h] = 0x05;
        page[h + 3..h + 5].copy_from_slice(&(cells as u16).to_be_bytes());
        page[h + 8..h + 12].copy_from_slice(&children[cells].to_be_bytes());
        for (k, child) in children[..cells].iter().enumerate() {
            let at = 2000 + 8 * k;
            page[h + 12 + 2 * k..h + 14 + 2 * k].copy_from_slice(&(at as u16).to_be_bytes());
            page[at..at + 4].copy_from_slice(&child.to_be_bytes());
        }
        page
    }

    fn leaf() -> Vec<u8> {
        let mut page = vec![0u8; crate::PAGE_SIZE];
        page[0] = 0x0d;
        page
    }

    #[test]
    fn parses_interior_pages() {
        let kids = [10, 11, 12, 13];
        assert_eq!(
            interior_children(5, &interior(5, &kids)),
            Some(kids.to_vec())
        );
        assert_eq!(
            interior_children(1, &interior(1, &kids)),
            Some(kids.to_vec())
        );
        assert_eq!(interior_children(5, &leaf()), None);
    }

    #[test]
    fn point_lookups_fetch_nothing_ahead() {
        let mut ra = ReadAhead::default();
        let kids: Vec<u32> = (100..300).collect();
        assert!(ra.on_read(2, &interior(2, &kids), 256).is_empty());
        assert!(ra.on_read(150, &leaf(), 256).is_empty());
        assert!(ra.on_read(120, &leaf(), 256).is_empty());
        assert!(ra.on_read(260, &leaf(), 256).is_empty());
    }

    #[test]
    fn a_forward_scan_fetches_growing_batches() {
        let mut ra = ReadAhead::default();
        let kids: Vec<u32> = (100..300).collect();
        ra.on_read(2, &interior(2, &kids), 64);
        assert!(ra.on_read(100, &leaf(), 64).is_empty());
        // The second neighbouring leaf starts the read-ahead: the next 8.
        assert_eq!(ra.on_read(101, &leaf(), 64), (102..110).collect::<Vec<_>>());
        let mut fetched = 2 + 8;
        let mut batches = 1;
        for n in 102..300 {
            let got = ra.on_read(n, &leaf(), 64);
            if !got.is_empty() {
                assert_eq!(
                    got[0],
                    100 + fetched as u32,
                    "batches continue where the last ended"
                );
                fetched += got.len();
                batches += 1;
            }
        }
        assert_eq!(fetched, 200, "every child was fetched exactly once");
        assert!(batches <= 7, "{batches} batches for 200 leaves");
    }

    #[test]
    fn a_backward_scan_fetches_behind() {
        let mut ra = ReadAhead::default();
        let kids: Vec<u32> = (100..200).collect();
        ra.on_read(2, &interior(2, &kids), 64);
        ra.on_read(199, &leaf(), 64);
        assert_eq!(ra.on_read(198, &leaf(), 64), (190..198).collect::<Vec<_>>());
    }

    #[test]
    fn a_scan_entering_the_next_interior_page_fetches_its_first_children() {
        let mut ra = ReadAhead::default();
        ra.on_read(2, &interior(2, &[3, 4, 5]), 64);
        ra.on_read(3, &interior(3, &(100..110).collect::<Vec<_>>()), 64);
        for n in 100..110 {
            ra.on_read(n, &leaf(), 64);
        }
        // The root's children 3 and 4 were read in a row: the scan moved to the next interior
        // page, so its first leaves come at once (and interior page 5 with them).
        let got = ra.on_read(4, &interior(4, &(200..300).collect::<Vec<_>>()), 64);
        assert!(got.contains(&5));
        assert!(got.contains(&200) && got.contains(&207), "{got:?}");
    }
}
