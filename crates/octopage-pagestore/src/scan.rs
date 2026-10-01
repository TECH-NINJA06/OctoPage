use std::collections::BTreeMap;

use octopage_git::{EntryMode, ObjectId, Transport};

use crate::codec::{self, Kind};
use crate::error::{Error, Result};
use crate::layout;
use crate::page::{Page, PageId, PageType, map_bit};
use crate::store::Snapshot;

/// What an integrity scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The commit scanned.
    pub commit: Option<ObjectId>,
    /// Pages checked.
    pub pages: usize,
    /// Their stored size, in bytes.
    pub bytes: u64,
    /// Whether pages were opened and parsed. `false` for an encrypted database scanned without
    /// its key: then only their framing was checked.
    pub decoded: bool,
    /// Everything wrong, one line each. Empty means the snapshot is sound.
    pub problems: Vec<String>,
}

impl Scan {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Pages fetched per round trip.
const BATCH: usize = 512;

impl<T: Transport + 'static> Snapshot<T> {
    /// Check every page of this snapshot (see the module docs).
    pub async fn scan(&self) -> Result<Scan> {
        let store = &self.store;
        let decoded = !store.is_locked();
        let mut scan = Scan {
            commit: Some(self.id()),
            decoded,
            ..Scan::default()
        };
        let pages = self.page_blobs(&mut scan.problems).await?;
        let sb = self.superblock();
        let epoch = self.epoch();
        let page_size = self.page_size();
        let ids: Vec<(PageId, ObjectId)> = pages.iter().map(|(id, blob)| (*id, *blob)).collect();
        let mut maps: BTreeMap<PageId, Page> = BTreeMap::new();
        for chunk in ids.chunks(BATCH) {
            let blobs: Vec<ObjectId> = chunk.iter().map(|(_, blob)| *blob).collect();
            let objects = match store.objects(&blobs).await {
                Ok(objects) => objects,
                Err(Error::Transport(e)) => {
                    scan.problems
                        .push(format!("page blobs could not be fetched: {e}"));
                    continue;
                }
                Err(e) => return Err(e),
            };
            for ((id, _), object) in chunk.iter().zip(objects) {
                scan.pages += 1;
                scan.bytes += object.data().len() as u64;
                let stored = object.data();
                if *id == PageId::SUPERBLOCK {
                    // Checked when the snapshot was loaded; stored as it is.
                    continue;
                }
                if !decoded {
                    if let Some(problem) = codec::frame_problem(stored) {
                        scan.problems.push(format!("page {id}: {problem}"));
                    }
                    continue;
                }
                let parsed = store
                    .codec()
                    .open(Kind::Page, stored)
                    .and_then(|bytes| Page::parse(bytes, page_size, *id, epoch));
                match parsed {
                    Ok(page) => {
                        if page.page_type() == PageType::FreeMap {
                            maps.insert(*id, page);
                        } else if sb.is_system(*id) {
                            scan.problems.push(format!(
                                "page {id} is a free-map id but holds a {:?} page",
                                page.page_type()
                            ));
                        }
                    }
                    Err(e) => scan.problems.push(format!("page {id}: {e}")),
                }
            }
        }
        if decoded {
            // The free-map lists exactly the pages that exist.
            for id in sb.first_data_page()..sb.next_free {
                let id = PageId::new(id)?;
                let (map, bit) = sb.map_slot(id);
                let used = maps.get(&map).is_some_and(|m| map_bit(m.payload(), bit));
                match (used, pages.contains_key(&id)) {
                    (true, false) => scan
                        .problems
                        .push(format!("page {id} is marked in use but does not exist")),
                    (false, true) => scan
                        .problems
                        .push(format!("page {id} exists but is marked free")),
                    _ => {}
                }
            }
            if let Some(id) = pages.keys().find(|id| id.get() >= sb.next_free) {
                scan.problems.push(format!(
                    "page {id} is beyond the superblock's high-water mark {}",
                    sb.next_free
                ));
            }
            if let Err(e) = self.catalog().await {
                scan.problems.push(format!("the catalog: {e}"));
            }
            if let Err(e) = self.changelog().await {
                scan.problems.push(format!("the changelog: {e}"));
            }
        }
        if let Some(keys) = self.root().keys {
            match store.object(keys).await {
                Ok(object) => {
                    if let Err(e) = codec::KeyFile::parse(object.data()) {
                        scan.problems.push(format!("the keys blob: {e}"));
                    }
                }
                Err(e) => scan.problems.push(format!("the keys blob: {e}")),
            }
        } else if sb.codec & crate::page::CODEC_ENCRYPTED != 0 {
            scan.problems
                .push("the database is encrypted but has no keys blob".into());
        }
        Ok(scan)
    }

    /// Every page and its blob, from the page map; malformed entries go to `problems`.
    async fn page_blobs(&self, problems: &mut Vec<String>) -> Result<BTreeMap<PageId, ObjectId>> {
        let mut pages = BTreeMap::new();
        let mut stack = vec![(self.info.pages(), 0usize, [0u8; 3])];
        while let Some((tree, depth, prefix)) = stack.pop() {
            let cached = self.store.tree(tree).await?;
            for entry in cached.tree.entries() {
                let Some(byte) = layout::name_byte(&entry.name) else {
                    problems.push(format!(
                        "page map: unexpected entry {:?}",
                        String::from_utf8_lossy(&entry.name)
                    ));
                    continue;
                };
                let mut next = prefix;
                next[depth] = byte;
                let want = if depth == 2 {
                    EntryMode::Blob
                } else {
                    EntryMode::Tree
                };
                if entry.mode != want {
                    problems.push(format!(
                        "page map: {} is a {:?}, expected a {want:?}",
                        hex(&next[..=depth]),
                        entry.mode
                    ));
                    continue;
                }
                if depth == 2 {
                    pages.insert(PageId::from_bytes(next), entry.id);
                } else {
                    stack.push((entry.id, depth + 1, next));
                }
            }
        }
        Ok(pages)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("/")
}
