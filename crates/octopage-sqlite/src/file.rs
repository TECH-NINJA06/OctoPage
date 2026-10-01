use std::collections::{BTreeMap, HashSet};
use std::ffi::c_int;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use octopage_git::{ObjectId, Transport};
use octopage_pagestore::{Outcome, Page, PageBuf, PageId, PageType, Rebase, Snapshot, WriteTxn};
use rusqlite::ffi;

use crate::readahead::ReadAhead;
use crate::{
    BLOCKED, Blocked, CONFLICT, Committed, Database, OUTCOME_UNKNOWN, RESERVED, Stats, catalog,
};

/// Attempts to rebase and commit after lost races before refusing the commit.
const MAX_REBASES: u32 = 8;

thread_local! {
    /// The page-store error behind the last I/O error this thread's VFS calls returned: SQLite
    /// passes on only the code, so the layer above picks the cause up here.
    static LAST_ERROR: std::cell::RefCell<Option<octopage_pagestore::Error>> =
        const { std::cell::RefCell::new(None) };
}

fn remember(error: octopage_pagestore::Error) {
    LAST_ERROR.with(|last| *last.borrow_mut() = Some(error));
}

/// The page-store error behind the last I/O error on this thread, once.
pub(crate) fn take_last_error() -> Option<octopage_pagestore::Error> {
    LAST_ERROR.with(|last| last.borrow_mut().take())
}

fn io_error(error: octopage_pagestore::Error) -> c_int {
    tracing::warn!(%error, "page store error in the SQLite VFS");
    remember(error);
    ffi::SQLITE_IOERR
}

/// A stored page as SQLite sees it: its payload, then the reserved bytes (zero).
fn to_sqlite(page: &Page) -> Vec<u8> {
    let payload = page.payload();
    let mut bytes = vec![0u8; payload.len() + RESERVED];
    bytes[..payload.len()].copy_from_slice(payload);
    bytes
}

/// A page SQLite wrote, to store: everything but the reserved bytes.
fn to_page(bytes: &[u8]) -> PageBuf {
    let mut page = PageBuf::new(PageType::Data, bytes.len());
    page.payload_mut()
        .copy_from_slice(&bytes[..bytes.len() - RESERVED]);
    page
}

/// Page 1 without the fields the VFS derives: change counter and database size (24..32),
/// version-valid-for and SQLite version (92..100).
fn normalized(page1: &[u8]) -> Vec<u8> {
    let mut bytes = page1[..page1.len() - RESERVED].to_vec();
    bytes[24..32].fill(0);
    bytes[92..100].fill(0);
    bytes
}

/// Refuse pragmas that would break the VFS's assumptions, or raise the database's page limit
/// (`max_pages`). Reading a pragma is always allowed.
pub(crate) fn check_pragma(
    pragma: &str,
    value: Option<&str>,
    page_size: usize,
    max_pages: Option<u32>,
) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    let value = value.trim().trim_matches(['\'', '"']).to_ascii_lowercase();
    let refuse = |why: &str| {
        Err(format!(
            "PRAGMA {pragma}={value} is not supported by OctoPage: {why}"
        ))
    };
    match pragma.to_ascii_lowercase().as_str() {
        "journal_mode" if value != "memory" => {
            refuse("commits are atomic on their own, so SQLite's journal stays in memory")
        }
        "synchronous" if value == "off" || value == "0" => {
            refuse("SQLite would never sync, and syncing is what commits")
        }
        "locking_mode" if value == "exclusive" => {
            refuse("SQLite would keep its page cache while other clients commit")
        }
        "page_size" if value != page_size.to_string() => refuse(&format!(
            "this database's pages are {page_size} bytes (`octopage migrate --page-size` makes \
             a copy with other pages)"
        )),
        "max_page_count"
            if max_pages.is_some_and(|max| {
                value
                    .parse::<i64>()
                    .map_or(true, |v| v <= 0 || v > max as i64)
            }) =>
        {
            refuse("the database's live-size limit sets it; change the limit in its settings")
        }
        _ => Ok(()),
    }
}

/// The main database file of one connection.
pub(crate) struct Main<T: Transport + 'static> {
    db: Arc<Database<T>>,
    lock: c_int,
    txn: Option<WriteTxn<T>>,
    /// Pages SQLite wrote in this transaction, by SQLite page number (page 1 included).
    pending: BTreeMap<u32, Vec<u8>>,
    /// Pages read from the page store in this transaction.
    read: HashSet<u32>,
    /// The change counter shown to SQLite in this transaction.
    counter: u32,
    /// Page-store id of SQLite's page 0 (SQLite pages start at 1).
    offset: u32,
    /// The file's size in pages as this transaction sees it, and as its base had it.
    size: u32,
    base_size: u32,
    /// The commit failed with this code: ignore SQLite's rollback writes and refuse any further
    /// sync until the transaction ends.
    failed: Option<c_int>,
    /// The snapshot SQLite's page cache reflects. It moves only where SQLite re-validates its
    /// cache (taking a SHARED lock from NONE) or to this connection's own commit.
    view: Option<ObjectId>,
    /// This transaction was started outside a fresh SHARED lock, so SQLite may be serving pages
    /// from its cache without reading them through the VFS: its read set may be incomplete.
    lazy: bool,
    ahead: ReadAhead,
    /// The changelog for the next commit, from the layer above. Deliberately kept across
    /// `end`: it is set before an autocommit statement takes its lock.
    changelog: Option<Vec<u8>>,
    /// What the last commit did, for the layer above.
    last: Option<Committed>,
    /// SQLite's pages of the commit being published: to say what a blocked page holds.
    committing: BTreeMap<u32, Vec<u8>>,
    /// Where push protection found a secret in the last refused commit.
    blocked: Option<Blocked>,
}

impl<T: Transport + 'static> Main<T> {
    /// The database's page limit, if it has one.
    pub(crate) fn max_pages(&self) -> Option<u32> {
        self.db.options.max_pages
    }

    /// SQLite's page size, which is the page store's.
    pub(crate) fn page_size(&self) -> usize {
        self.db.page_size
    }

    pub(crate) fn new(db: Arc<Database<T>>) -> Self {
        Main {
            db,
            lock: ffi::SQLITE_LOCK_NONE,
            txn: None,
            pending: BTreeMap::new(),
            read: HashSet::new(),
            counter: 0,
            offset: 0,
            size: 0,
            base_size: 0,
            failed: None,
            view: None,
            lazy: false,
            ahead: ReadAhead::default(),
            changelog: None,
            last: None,
            committing: BTreeMap::new(),
            blocked: None,
        }
    }

    pub(crate) fn last_blocked(&self) -> Option<Blocked> {
        self.blocked.clone()
    }

    pub(crate) fn set_changelog(&mut self, changelog: Option<Vec<u8>>) {
        self.changelog = changelog;
    }

    pub(crate) fn last_commit(&self) -> Option<Committed> {
        self.last
    }

    // ------------------------------------------------------------------ SQLite's calls

    pub(crate) fn read(&mut self, out: &mut [u8], offset: u64) -> c_int {
        if let Err(rc) = self.begin(false) {
            return rc;
        }
        let (start, end) = (offset as usize, offset as usize + out.len());
        let mut pos = start;
        let mut short = false;
        let page_size = self.db.page_size;
        while pos < end {
            let n = (pos / page_size + 1) as u32;
            let within = pos % page_size;
            let take = (page_size - within).min(end - pos);
            match self.read_page(n) {
                Ok(Some(page)) => out[pos - start..pos - start + take]
                    .copy_from_slice(&page[within..within + take]),
                Ok(None) => short = true,
                Err(rc) => return rc,
            }
            pos += take;
        }
        if short {
            ffi::SQLITE_IOERR_SHORT_READ
        } else {
            ffi::SQLITE_OK
        }
    }

    pub(crate) fn write(&mut self, data: &[u8], offset: u64) -> Result<(), c_int> {
        if self.failed.is_some() {
            return Ok(()); // SQLite rolling back a failed commit: nothing to undo here
        }
        if self.db.at.is_some() {
            return Err(ffi::SQLITE_READONLY);
        }
        let page_size = self.db.page_size;
        if data.len() != page_size || !(offset as usize).is_multiple_of(page_size) {
            return Err(ffi::SQLITE_IOERR_WRITE);
        }
        self.begin(false)?;
        let n = (offset as usize / page_size + 1) as u32;
        self.id(n)?;
        self.pending.insert(n, data.to_vec());
        self.size = self.size.max(n);
        Stats::bump(&self.db.stats.pages_written);
        Ok(())
    }

    pub(crate) fn truncate(&mut self, bytes: u64) -> Result<(), c_int> {
        if self.failed.is_some() {
            return Ok(());
        }
        self.begin(false)?;
        let pages = bytes.div_ceil(self.db.page_size as u64) as u32;
        self.pending.retain(|&n, _| n <= pages);
        self.size = pages;
        Ok(())
    }

    pub(crate) fn sync(&mut self) -> Result<(), c_int> {
        if let Some(rc) = self.failed {
            return Err(rc); // stays refused until SQLite ends the transaction
        }
        if self.txn.is_none() || (self.pending.is_empty() && self.size == self.base_size) {
            return Ok(());
        }
        self.commit()
    }

    pub(crate) fn size(&mut self) -> Result<u64, c_int> {
        self.begin(false)?;
        Ok(self.size as u64 * self.db.page_size as u64)
    }

    pub(crate) fn lock(&mut self, level: c_int) -> Result<(), c_int> {
        if self.lock == ffi::SQLITE_LOCK_NONE && level > ffi::SQLITE_LOCK_NONE {
            // A new SQLite transaction, which re-validates SQLite's cache: a fresh snapshot.
            self.end();
            self.begin(true)?;
        }
        if self.db.at.is_some() && level >= ffi::SQLITE_LOCK_RESERVED {
            return Err(ffi::SQLITE_READONLY);
        }
        self.lock = level;
        Ok(())
    }

    pub(crate) fn unlock(&mut self, level: c_int) {
        if level == ffi::SQLITE_LOCK_NONE {
            self.end();
            self.view = None;
        }
        self.lock = level;
    }

    // ------------------------------------------------------------------ transactions

    /// Start a page-store transaction. `fresh`: SQLite is about to re-validate its cache, so the
    /// transaction may start from the newest head. Otherwise it must start from the snapshot
    /// SQLite's cache reflects, or SQLite would write stale pages on top of a newer base.
    fn begin(&mut self, fresh: bool) -> Result<(), c_int> {
        if self.txn.is_some() {
            return Ok(());
        }
        let db = self.db.clone();
        let view = if fresh { None } else { self.view };
        let txn = db
            .block_on(async {
                match (db.at, view) {
                    (Some(commit), _) | (None, Some(commit)) => db.store.begin_at(commit).await,
                    (None, None) => {
                        if db.options.refresh_on_begin {
                            db.store.refresh().await?;
                        }
                        db.store.begin().await
                    }
                }
            })
            .map_err(io_error)?;
        if txn.page_size() != db.page_size {
            // The database moved to a new generation with other pages (`octopage migrate`).
            return Err(io_error(octopage_pagestore::Error::Invalid(format!(
                "the database now has {}-byte pages (it was migrated): open it again",
                txn.page_size()
            ))));
        }
        self.offset = txn.base().superblock().map_pages;
        let max = db.block_on(txn.base().max_page()).map_err(io_error)?.get();
        self.base_size = max.saturating_sub(self.offset);
        self.size = self.base_size;
        if fresh {
            // A new counter for every fresh transaction, so SQLite drops its page cache and
            // re-reads every page through the VFS (complete read sets). SQLite keeps its cache when
            // the counter equals the value it last wrote itself, which is the one we showed it plus
            // one; showing only even values, stepping by two, means the next one never matches.
            self.counter = db.counter.fetch_add(2, Ordering::Relaxed).wrapping_add(2) & !1;
        }
        self.lazy = !fresh;
        self.view = Some(txn.base().id());
        self.txn = Some(txn);
        self.pending.clear();
        self.read.clear();
        self.ahead = ReadAhead::default();
        // `failed` is deliberately left alone: after a refused commit SQLite rolls back by
        // rewriting the original pages, which may start a lazy transaction; those writes must
        // stay ignored until SQLite ends the transaction (`end`).
        Stats::bump(&db.stats.transactions);
        Ok(())
    }

    fn end(&mut self) {
        self.txn = None;
        self.pending.clear();
        self.read.clear();
        self.failed = None;
        self.lazy = false;
    }

    fn id(&self, n: u32) -> Result<PageId, c_int> {
        PageId::new(self.offset + n).map_err(|_| ffi::SQLITE_FULL)
    }

    /// Page 1 with the bookkeeping SQLite expects: this transaction's counter, the page count.
    fn present(&self, mut page1: Vec<u8>) -> Vec<u8> {
        page1[24..28].copy_from_slice(&self.counter.to_be_bytes());
        page1[28..32].copy_from_slice(&self.size.to_be_bytes());
        page1[92..96].copy_from_slice(&self.counter.to_be_bytes());
        page1
    }

    fn read_page(&mut self, n: u32) -> Result<Option<Vec<u8>>, c_int> {
        if n == 0 || n > self.size {
            return Ok(None);
        }
        if let Some(bytes) = self.pending.get(&n) {
            return Ok(Some(if n == 1 {
                self.present(bytes.clone())
            } else {
                bytes.clone()
            }));
        }
        let id = self.id(n)?;
        let db = self.db.clone();
        let txn = self.txn.as_mut().expect("reading inside a transaction");
        let page = db.block_on(txn.get(id)).map_err(io_error)?;
        Stats::bump(&db.stats.pages_read);
        self.read.insert(n);
        let Some(page) = page else {
            return Ok(None); // a page SQLite never wrote: it reads as zeros
        };
        let bytes = to_sqlite(&page);
        if db.options.read_ahead > 0 {
            let wanted = self.ahead.on_read(n, &bytes, db.options.read_ahead);
            if !wanted.is_empty() {
                self.prefetch(wanted);
            }
        }
        Ok(Some(if n == 1 { self.present(bytes) } else { bytes }))
    }

    /// Fetch pages a scan is about to read, in the background: SQLite's reads of them then find
    /// them cached or join the fetch in flight. They join the read set only when SQLite reads them.
    fn prefetch(&self, pages: Vec<u32>) {
        let Some(txn) = &self.txn else { return };
        let ids: Vec<PageId> = pages
            .into_iter()
            .filter(|n| {
                *n <= self.base_size && !self.pending.contains_key(n) && !self.read.contains(n)
            })
            .filter_map(|n| PageId::new(self.offset + n).ok())
            .collect();
        if ids.is_empty() {
            return;
        }
        let stats = &self.db.stats;
        Stats::bump(&stats.prefetch_batches);
        Stats::add(&stats.prefetched_pages, ids.len());
        let snapshot = txn.base().clone();
        self.db.runtime.spawn(async move {
            if let Err(error) = snapshot.get_many(&ids).await {
                tracing::debug!(%error, "read-ahead failed");
            }
        });
    }

    fn commit(&mut self) -> Result<(), c_int> {
        let mut txn = self.txn.take().expect("committing inside a transaction");
        self.blocked = None;
        let result = match self.stage(&mut txn) {
            Ok(()) => self.publish(txn),
            Err(rc) => Err(rc),
        };
        self.pending.clear();
        self.committing.clear();
        if let Err(rc) = result {
            self.failed = Some(rc);
        }
        result
    }

    /// Turn SQLite's writes into the page-store transaction's writes.
    fn stage(&mut self, txn: &mut WriteTxn<T>) -> Result<(), c_int> {
        let db = self.db.clone();
        // SQLite shrinks the file (VACUUM, auto-vacuum) only in the second phase of its commit,
        // after the sync that publishes it. So take the final size from SQLite's own record in
        // page 1: bytes 28..32, valid when bytes 92..96 match the change counter at 24..28.
        if let Some(page1) = self.pending.get(&1) {
            let field = |at: usize| u32::from_be_bytes(page1[at..at + 4].try_into().unwrap());
            let size = field(28);
            if size > 0 && field(24) == field(92) {
                self.pending.retain(|&n, _| n <= size);
                self.size = size;
            }
        }
        // Pages a truncation cut off.
        for n in (self.size + 1)..=self.base_size {
            let id = self.id(n)?;
            if db.block_on(txn.exists(id)).map_err(io_error)? {
                txn.free(id).map_err(io_error)?;
            }
        }
        let pending = std::mem::take(&mut self.pending);
        let mut schema_changed = false;
        for (&n, bytes) in &pending {
            let id = self.id(n)?;
            // Compare with the base and leave out pages SQLite rewrote unchanged: page 1 always
            // (only its derived fields change on most commits), other pages only if this
            // transaction read them. Those are cached, and dropping a blind write would change
            // its meaning, while a read page stays in the read set.
            let base = if n == 1 || self.read.contains(&n) {
                db.block_on(txn.base().get(id))
                    .map_err(io_error)?
                    .map(|p| to_sqlite(&p))
            } else {
                None
            };
            if let Some(base) = &base {
                let same = if n == 1 {
                    normalized(base) == normalized(bytes)
                } else {
                    base[..base.len() - RESERVED] == bytes[..bytes.len() - RESERVED]
                };
                if same {
                    Stats::bump(&db.stats.unchanged_pages);
                    continue;
                }
            }
            if n == 1 {
                Stats::bump(&db.stats.page1_stored);
                // Bytes 40..44 are the schema cookie, which every schema change bumps.
                schema_changed = base.as_ref().map(|b| &b[40..44]) != Some(&bytes[40..44]);
            }
            if !db.block_on(txn.exists(id)).map_err(io_error)? {
                db.block_on(txn.claim(id)).map_err(io_error)?;
            }
            txn.put(id, to_page(bytes)).map_err(io_error)?;
        }
        // The file's last page must exist: the size shown to SQLite is derived from the highest
        // page in the page map. (SQLite can leave a page it put on the free list unwritten.)
        if self.size > 0 {
            let last = self.id(self.size)?;
            if !db.block_on(txn.exists(last)).map_err(io_error)? {
                db.block_on(txn.claim(last)).map_err(io_error)?;
                txn.put(last, to_page(&vec![0u8; db.page_size]))
                    .map_err(io_error)?;
            }
        }
        if schema_changed {
            self.write_catalog(txn, &pending[&1]);
        }
        if let Some(changelog) = self.changelog.take() {
            txn.set_changelog(changelog);
        }
        self.committing = pending;
        Ok(())
    }

    /// Describe the places push protection blocked: the tables and rows their pages hold.
    fn describe_blocked(
        &self,
        base: &Snapshot<T>,
        pages: &[PageId],
        others: &[String],
        message: String,
    ) -> Blocked {
        let offset = self.offset;
        let wanted: Vec<u32> = pages
            .iter()
            .filter_map(|p| p.get().checked_sub(offset))
            .filter(|n| *n > 0)
            .collect();
        let read = |n: u32| -> Result<Vec<u8>, String> {
            if let Some(bytes) = self.committing.get(&n) {
                return Ok(bytes.clone());
            }
            let id = PageId::new(offset + n).map_err(|e| e.to_string())?;
            match self.db.block_on(base.get(id)) {
                Ok(Some(page)) => Ok(to_sqlite(&page)),
                Ok(None) => Err(format!("page {n} is missing")),
                Err(e) => Err(e.to_string()),
            }
        };
        let usable = self.db.page_size - RESERVED;
        let found = crate::locate::locate(&wanted, usable, read).unwrap_or_else(|error| {
            tracing::debug!(%error, "could not place the blocked pages");
            Default::default()
        });
        let mut places: Vec<String> = wanted
            .iter()
            .map(|n| found.get(n).cloned().unwrap_or_else(|| format!("page {n}")))
            .collect();
        places.extend(others.iter().map(|other| match other.as_str() {
            "changelog" => "the changelog (a statement or its parameters)".to_string(),
            "catalog" => "the catalog (the schema)".to_string(),
            other => other.to_string(),
        }));
        Blocked { places, message }
    }

    /// After a commit whose outcome was unknown: read the head a few times, pausing in between,
    /// until it says whether the commit landed.
    fn settle(&self, base: ObjectId, commit: ObjectId) -> Option<bool> {
        for attempt in 0..3 {
            match self.db.block_on(self.db.store.settle(base, commit)) {
                Ok(Some(landed)) => return Some(landed),
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "settling a commit failed"),
            }
            std::thread::sleep(Duration::from_millis(250 << attempt));
        }
        None
    }

    /// Rewrite the catalog blob from the schema pages as this transaction leaves them.
    fn write_catalog(&self, txn: &mut WriteTxn<T>, page1: &[u8]) {
        let db = &self.db;
        let offset = self.offset;
        let cookie = u32::from_be_bytes(page1[40..44].try_into().unwrap());
        let objects = catalog::read_schema(self.db.page_size - RESERVED, |n| {
            let id = PageId::new(offset + n).map_err(|e| e.to_string())?;
            match db.block_on(txn.get(id)) {
                Ok(Some(page)) => Ok(to_sqlite(&page)),
                Ok(None) => Err(format!("schema page {n} is missing")),
                Err(e) => Err(e.to_string()),
            }
        });
        match objects.and_then(|objects| catalog::encode(cookie, objects)) {
            Ok(json) => {
                txn.set_catalog(json);
                Stats::bump(&db.stats.catalog_updates);
            }
            Err(error) => {
                tracing::warn!(%error, "could not build the catalog blob; committing without it");
                Stats::bump(&db.stats.catalog_errors);
            }
        }
    }

    /// Commit, rebasing after lost races whose pages do not overlap ours.
    fn publish(&mut self, mut txn: WriteTxn<T>) -> Result<(), c_int> {
        let db = self.db.clone();
        for _ in 0..MAX_REBASES {
            let snapshot = txn.base().clone();
            let base = snapshot.id();
            match db.block_on(txn.commit()) {
                Err(octopage_pagestore::Error::SecretBlocked {
                    pages,
                    others,
                    message,
                }) => {
                    tracing::warn!(%message, "push protection blocked the commit");
                    self.blocked = Some(self.describe_blocked(&snapshot, &pages, &others, message));
                    Stats::bump(&db.stats.failed);
                    return Err(BLOCKED);
                }
                Err(octopage_pagestore::Error::OutcomeUnknown {
                    base,
                    commit,
                    reason,
                }) => {
                    tracing::warn!(%reason, %commit, "commit outcome unknown; settling it");
                    return match self.settle(base, commit) {
                        Some(true) => {
                            Stats::bump(&db.stats.settled);
                            self.committed(commit, true);
                            Ok(())
                        }
                        Some(false) => {
                            // Another commit moved the head, so ours can never land.
                            Stats::bump(&db.stats.settled);
                            Stats::bump(&db.stats.conflicts);
                            Err(CONFLICT)
                        }
                        None => {
                            Stats::bump(&db.stats.unknown_outcomes);
                            Err(OUTCOME_UNKNOWN)
                        }
                    };
                }
                Err(error) => {
                    tracing::warn!(%error, "commit failed; nothing was applied");
                    Stats::bump(&db.stats.failed);
                    remember(error);
                    return Err(ffi::SQLITE_IOERR);
                }
                Ok(Outcome::Committed(c)) => {
                    self.committed(c.head, c.head != base);
                    return Ok(());
                }
                // A transaction whose reads SQLite may have served from its cache cannot prove
                // its read set is complete, so it is never rebased.
                Ok(Outcome::Conflict(_)) if self.lazy => break,
                Ok(Outcome::Conflict(lost)) => match db.block_on(lost.txn.rebase(lost.head)) {
                    Ok(Rebase::Ready(rebased)) => {
                        Stats::bump(&db.stats.rebases);
                        txn = rebased;
                    }
                    Ok(Rebase::Overlap { .. }) => break,
                    // The race was lost, so nothing landed.
                    Err(error) => return Err(io_error(error)),
                },
            }
        }
        Stats::bump(&db.stats.conflicts);
        Err(CONFLICT)
    }

    fn committed(&mut self, head: ObjectId, published: bool) {
        let stats = &self.db.stats;
        Stats::bump(&stats.commits);
        if published {
            Stats::bump(&stats.published);
        }
        self.view = Some(head); // SQLite's cache now reflects this commit
        self.last = Some(Committed { head, published });
    }
}

#[cfg(test)]
mod tests {
    use super::check_pragma;

    #[test]
    fn pragmas_that_would_break_commits_are_refused() {
        assert!(check_pragma("journal_mode", Some("WAL"), 4096, None).is_err());
        assert!(check_pragma("journal_mode", Some("delete"), 4096, None).is_err());
        assert!(check_pragma("journal_mode", Some("'memory'"), 4096, None).is_ok());
        assert!(check_pragma("journal_mode", None, 4096, None).is_ok());
        assert!(check_pragma("synchronous", Some("OFF"), 4096, None).is_err());
        assert!(check_pragma("synchronous", Some("0"), 4096, None).is_err());
        assert!(check_pragma("synchronous", Some("normal"), 4096, None).is_ok());
        assert!(check_pragma("locking_mode", Some("EXCLUSIVE"), 4096, None).is_err());
        assert!(check_pragma("page_size", Some("8192"), 4096, None).is_err());
        assert!(check_pragma("page_size", Some("4096"), 4096, None).is_ok());
        assert!(check_pragma("page_size", Some("8192"), 8192, None).is_ok());
        assert!(check_pragma("max_page_count", Some("100"), 4096, Some(50)).is_err());
        assert!(check_pragma("max_page_count", Some("50"), 4096, Some(50)).is_ok());
        assert!(check_pragma("max_page_count", Some("10"), 4096, Some(50)).is_ok());
        assert!(check_pragma("max_page_count", Some("100"), 4096, None).is_ok());
        assert!(check_pragma("cache_size", Some("-2000"), 4096, None).is_ok());
    }
}
