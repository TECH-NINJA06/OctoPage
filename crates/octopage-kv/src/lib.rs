pub mod soak;

use std::sync::Arc;

use octopage_git::{ObjectId, Transport};
use octopage_pagestore::{Outcome, Page, PageBuf, PageId, PageStore, PageType, Rebase, WriteTxn};

pub const MAX_KEY: usize = 255;
pub const MAX_VALUE: usize = 1024;
/// Attempts per transaction before it fails with a serialization error.
pub const MAX_ATTEMPTS: u32 = 8;

#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Store(#[from] octopage_pagestore::Error),
    /// Other writers kept changing the pages this transaction needed; nothing was committed.
    #[error("gave up after {} attempts: other writers kept changing the same pages", .0.attempts)]
    Serialization(Stats),
    #[error("key or value too large (keys up to {MAX_KEY} bytes, values up to {MAX_VALUE})")]
    TooLarge,
    #[error("not a key-value database: {0}")]
    NotKv(String),
    #[error("value of {0:?} is not an integer")]
    NotInteger(String),
    #[error("verification failed: {0}")]
    Verify(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A statement. `Add` treats the value as a decimal integer (missing = 0) and returns the sum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Get(Vec<u8>),
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Add(Vec<u8>, i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpResult {
    Value(Option<Vec<u8>>),
    Done,
    Deleted(bool),
    Counter(i64),
}

/// How a transaction got committed.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Commit attempts, including the successful one.
    pub attempts: u32,
    /// Lost races resolved by moving the writes onto the new head.
    pub rebases: u32,
    /// Lost races that required running the statements again.
    pub reexecutions: u32,
    pub head: ObjectId,
}

fn show(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

impl Op {
    fn changelog_line(&self) -> String {
        match self {
            Op::Get(k) => format!("get {}", show(k)),
            Op::Put(k, v) => format!("put {} {}", show(k), show(v)),
            Op::Delete(k) => format!("delete {}", show(k)),
            Op::Add(k, n) => format!("add {} {n}", show(k)),
        }
    }
}

// ------------------------------------------------------------------------ bucket pages
//
// Payload: [0..4) next overflow page id (0 = none), then entries of
// [u16 key length][key][u32 value length][value]. The header's free-space offset marks the end.

const NEXT: usize = 4;

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn parse_bucket(page: &Page) -> Result<(u32, Entries)> {
    let payload = page.payload();
    let bad = || Error::NotKv(format!("page {} is not a well-formed bucket", page.id()));
    let next = u32::from_le_bytes(payload[..NEXT].try_into().unwrap());
    let end = page.free_offset() as usize;
    if end < NEXT || end > payload.len() {
        return Err(bad());
    }
    let mut entries = Vec::with_capacity(page.cell_count() as usize);
    let mut at = NEXT;
    while at < end {
        let klen = u16::from_le_bytes(payload.get(at..at + 2).ok_or_else(bad)?.try_into().unwrap())
            as usize;
        let key = payload.get(at + 2..at + 2 + klen).ok_or_else(bad)?.to_vec();
        at += 2 + klen;
        let vlen = u32::from_le_bytes(payload.get(at..at + 4).ok_or_else(bad)?.try_into().unwrap())
            as usize;
        let value = payload.get(at + 4..at + 4 + vlen).ok_or_else(bad)?.to_vec();
        at += 4 + vlen;
        entries.push((key, value));
    }
    Ok((next, entries))
}

fn entries_size(entries: &Entries) -> usize {
    entries.iter().map(|(k, v)| 6 + k.len() + v.len()).sum()
}

fn bucket_page(page_size: usize, next: u32, entries: &Entries) -> Option<PageBuf> {
    let mut page = PageBuf::new(PageType::Data, page_size);
    if NEXT + entries_size(entries) > page.payload().len() {
        return None;
    }
    let payload = page.payload_mut();
    payload[..NEXT].copy_from_slice(&next.to_le_bytes());
    let mut at = NEXT;
    for (k, v) in entries {
        payload[at..at + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
        payload[at + 2..at + 2 + k.len()].copy_from_slice(k);
        at += 2 + k.len();
        payload[at..at + 4].copy_from_slice(&(v.len() as u32).to_le_bytes());
        payload[at + 4..at + 4 + v.len()].copy_from_slice(v);
        at += 4 + v.len();
    }
    page.set_free_offset(at as u32);
    page.set_cell_count(entries.len() as u16);
    Some(page)
}

fn fnv1a(key: &[u8]) -> u64 {
    key.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn catalog_json(buckets: &[PageId]) -> String {
    let ids: Vec<u32> = buckets.iter().map(|b| b.get()).collect();
    serde_json::json!({ "kv": { "version": 1, "buckets": ids } }).to_string()
}

fn parse_catalog(catalog: Option<&[u8]>) -> Result<Arc<[PageId]>> {
    let catalog = catalog.ok_or_else(|| Error::NotKv("the database has no catalog".into()))?;
    let value: serde_json::Value =
        serde_json::from_slice(catalog).map_err(|e| Error::NotKv(e.to_string()))?;
    let ids = value["kv"]["buckets"]
        .as_array()
        .ok_or_else(|| Error::NotKv("the catalog lists no buckets".into()))?;
    ids.iter()
        .map(|id| {
            id.as_u64()
                .and_then(|n| PageId::new(n as u32).ok())
                .ok_or_else(|| Error::NotKv("bad bucket id".into()))
        })
        .collect()
}

// ------------------------------------------------------------------------ transactions

/// The statements' view of the database inside one transaction attempt.
pub struct KvTxn<T: Transport + 'static> {
    txn: WriteTxn<T>,
    buckets: Arc<[PageId]>,
}

impl<T: Transport + 'static> KvTxn<T> {
    /// The chain of pages for `key`'s bucket, with their decoded contents.
    async fn chain(&mut self, key: &[u8]) -> Result<Vec<(PageId, u32, Entries)>> {
        let mut id = self.buckets[(fnv1a(key) % self.buckets.len() as u64) as usize];
        let mut chain = Vec::new();
        loop {
            let page = self
                .txn
                .get(id)
                .await?
                .ok_or_else(|| Error::NotKv(format!("bucket page {id} is missing")))?;
            let (next, entries) = parse_bucket(&page)?;
            chain.push((id, next, entries));
            if next == 0 {
                return Ok(chain);
            }
            id = PageId::new(next)?;
        }
    }

    pub async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let chain = self.chain(key).await?;
        Ok(chain
            .into_iter()
            .flat_map(|(_, _, e)| e)
            .find(|(k, _)| k == key)
            .map(|(_, v)| v))
    }

    pub async fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY || value.len() > MAX_VALUE {
            return Err(Error::TooLarge);
        }
        let mut chain = self.chain(key).await?;
        let mut dirty = vec![false; chain.len()];
        for (i, (_, _, entries)) in chain.iter_mut().enumerate() {
            let before = entries.len();
            entries.retain(|(k, _)| k != key);
            dirty[i] |= entries.len() != before;
        }
        let entry = (key.to_vec(), value.to_vec());
        let size = self.txn.page_size();
        let room = |entries: &Entries| {
            NEXT + entries_size(entries) + 6 + key.len() + value.len()
                <= size - octopage_pagestore::page::HEADER_LEN
        };
        match chain.iter().position(|(_, _, e)| room(e)) {
            Some(i) => {
                chain[i].2.push(entry);
                dirty[i] = true;
            }
            None => {
                // Every page in the chain is full: link a new overflow page at the end.
                let new = self.txn.allocate().await?;
                let last = chain.len() - 1;
                chain[last].1 = new.get();
                dirty[last] = true;
                chain.push((new, 0, vec![entry]));
                dirty.push(true);
            }
        }
        for ((id, next, entries), dirty) in chain.iter().zip(dirty) {
            if dirty {
                self.txn.put(
                    *id,
                    bucket_page(size, *next, entries).expect("sized to fit"),
                )?;
            }
        }
        Ok(())
    }

    pub async fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let size = self.txn.page_size();
        for (id, next, mut entries) in self.chain(key).await? {
            let before = entries.len();
            entries.retain(|(k, _)| k != key);
            if entries.len() != before {
                self.txn.put(
                    id,
                    bucket_page(size, next, &entries).expect("smaller than before"),
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn run(&mut self, op: &Op) -> Result<OpResult> {
        Ok(match op {
            Op::Get(k) => OpResult::Value(self.get(k).await?),
            Op::Put(k, v) => {
                self.put(k, v).await?;
                OpResult::Done
            }
            Op::Delete(k) => OpResult::Deleted(self.delete(k).await?),
            Op::Add(k, n) => {
                let current = match self.get(k).await? {
                    None => 0,
                    Some(v) => std::str::from_utf8(&v)
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok())
                        .ok_or_else(|| Error::NotInteger(show(k)))?,
                };
                let sum = current + n;
                self.put(k, sum.to_string().as_bytes()).await?;
                OpResult::Counter(sum)
            }
        })
    }
}

// ------------------------------------------------------------------------ the store

pub struct Kv<T: Transport + 'static> {
    store: PageStore<T>,
    buckets: Arc<[PageId]>,
}

impl<T: Transport + 'static> Clone for Kv<T> {
    fn clone(&self) -> Self {
        Kv {
            store: self.store.clone(),
            buckets: self.buckets.clone(),
        }
    }
}

impl<T: Transport + 'static> Kv<T> {
    /// Lay out `buckets` empty bucket pages and the catalog in one commit.
    pub async fn create(store: PageStore<T>, buckets: usize) -> Result<Self> {
        let mut txn = store.begin().await?;
        if txn.catalog().await?.is_some() {
            return Err(Error::NotKv("the database already has a catalog".into()));
        }
        let mut ids = Vec::with_capacity(buckets);
        for _ in 0..buckets.max(1) {
            let id = txn.allocate().await?;
            txn.put(id, bucket_page(txn.page_size(), 0, &Vec::new()).unwrap())?;
            ids.push(id);
        }
        txn.set_catalog(catalog_json(&ids));
        txn.set_changelog(format!("kv init {} buckets", ids.len()));
        match txn.commit().await? {
            Outcome::Committed(_) => Ok(Kv {
                store,
                buckets: ids.into(),
            }),
            Outcome::Conflict(_) => Err(Error::NotKv(
                "another writer changed the database during init".into(),
            )),
        }
    }

    pub async fn open(store: PageStore<T>) -> Result<Self> {
        let catalog = store.latest().await?.catalog().await?;
        let buckets = parse_catalog(catalog.as_deref())?;
        Ok(Kv { store, buckets })
    }

    pub fn store(&self) -> &PageStore<T> {
        &self.store
    }

    pub fn buckets(&self) -> usize {
        self.buckets.len()
    }

    /// Run `ops` as one transaction: rebase when possible, re-execute when not.
    pub async fn apply(&self, ops: &[Op]) -> Result<(Vec<OpResult>, Stats)> {
        let mut stats = Stats::default();
        let mut txn = self.store.begin().await?;
        loop {
            let mut kv = KvTxn {
                txn,
                buckets: self.buckets.clone(),
            };
            let mut results = Vec::with_capacity(ops.len());
            for op in ops {
                results.push(kv.run(op).await?);
            }
            let mut pending = kv.txn;
            pending.set_changelog(
                ops.iter()
                    .map(Op::changelog_line)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            loop {
                stats.attempts += 1;
                if stats.attempts > MAX_ATTEMPTS {
                    stats.attempts = MAX_ATTEMPTS;
                    return Err(Error::Serialization(stats));
                }
                match pending.commit().await? {
                    Outcome::Committed(c) => {
                        stats.head = c.head;
                        return Ok((results, stats));
                    }
                    Outcome::Conflict(conflict) => {
                        match conflict.txn.rebase(conflict.head).await? {
                            Rebase::Ready(rebased) => {
                                stats.rebases += 1;
                                pending = rebased;
                            }
                            Rebase::Overlap { head, .. } => {
                                stats.reexecutions += 1;
                                // Randomised, growing back-off: without it the writer that just won
                                // (with everything cached) tends to win again and again.
                                let ceiling = 1u64 << stats.attempts.min(6);
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    fastrand::u64(0..=ceiling),
                                ))
                                .await;
                                txn = self.store.begin_at(head).await?;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    pub async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (mut results, _) = self.apply(&[Op::Get(key.to_vec())]).await?;
        match results.pop() {
            Some(OpResult::Value(v)) => Ok(v),
            _ => unreachable!(),
        }
    }

    pub async fn put(&self, key: &[u8], value: &[u8]) -> Result<Stats> {
        Ok(self
            .apply(&[Op::Put(key.to_vec(), value.to_vec())])
            .await?
            .1)
    }

    pub async fn delete(&self, key: &[u8]) -> Result<bool> {
        match self.apply(&[Op::Delete(key.to_vec())]).await?.0.pop() {
            Some(OpResult::Deleted(found)) => Ok(found),
            _ => unreachable!(),
        }
    }

    pub async fn add(&self, key: &[u8], n: i64) -> Result<i64> {
        match self.apply(&[Op::Add(key.to_vec(), n)]).await?.0.pop() {
            Some(OpResult::Counter(v)) => Ok(v),
            _ => unreachable!(),
        }
    }

    /// Every key starting with `prefix`, sorted, as of the known head. Bucket pages come in one fetch.
    pub async fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let snapshot = self.store.latest().await?;
        let mut out = Vec::new();
        let mut frontier: Vec<PageId> = self.buckets.to_vec();
        while !frontier.is_empty() {
            let pages = snapshot.get_many(&frontier).await?;
            frontier.clear();
            for page in pages.into_iter().flatten() {
                let (next, entries) = parse_bucket(&page)?;
                out.extend(entries.into_iter().filter(|(k, _)| k.starts_with(prefix)));
                if next != 0 {
                    frontier.push(PageId::new(next)?);
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_capacity() {
        let fits: Entries = (0..10)
            .map(|i| (format!("k{i}").into_bytes(), vec![i as u8; i]))
            .collect();
        let page = bucket_page(4096, 77, &fits).unwrap();
        assert_eq!(page.cell_count(), 10);
        assert_eq!(page.free_offset() as usize, NEXT + entries_size(&fits));
        let too_many: Entries = (0..10).map(|i| (vec![i as u8], vec![0u8; 500])).collect();
        assert!(bucket_page(4096, 0, &too_many).is_none());
    }

    #[test]
    fn catalog_round_trip() {
        let ids: Vec<PageId> = [518, 519, 600]
            .iter()
            .map(|n| PageId::new(*n).unwrap())
            .collect();
        assert_eq!(
            &*parse_catalog(Some(catalog_json(&ids).as_bytes())).unwrap(),
            &ids[..]
        );
        assert!(parse_catalog(None).is_err());
        assert!(parse_catalog(Some(b"{}")).is_err());
    }

    #[test]
    fn changelog_lines_are_readable() {
        assert_eq!(
            Op::Put(b"a b".to_vec(), b"\x01".to_vec()).changelog_line(),
            "put a b \\x01"
        );
        assert_eq!(
            Op::Add(b"total".to_vec(), -2).changelog_line(),
            "add total -2"
        );
    }
}
