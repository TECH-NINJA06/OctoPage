use std::ffi::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use octopage_git::{ObjectId, Transport};
use octopage_pagestore::PageStore;
use rusqlite::{Connection, OpenFlags, ffi};
use tokio::runtime::Handle;

mod catalog;
mod file;
mod locate;
mod readahead;
pub mod soak;
mod vfs;

pub use catalog::{Catalog, SchemaObject};

pub const PAGE_SIZE: usize = 4096;
/// Bytes SQLite leaves alone at the end of every page; OctoPage's page header takes them.
pub const RESERVED: usize = 32;

/// The commit was refused: another client's commit changed pages this transaction read or
/// wrote. Nothing was applied, and SQLite has rolled back and dropped its page cache, so the
/// application can simply run the transaction again. An extended `SQLITE_IOERR` code, because
/// an I/O error is what makes SQLite abandon the transaction cleanly.
pub const CONFLICT: c_int = ffi::SQLITE_IOERR | (201 << 8);

/// The network failed while the commit was being published, and reading the head a few times
/// afterwards did not settle whether it landed. Check (for example for a row the transaction
/// wrote) before running it again.
pub const OUTCOME_UNKNOWN: c_int = ffi::SQLITE_IOERR | (202 << 8);

/// GitHub's push protection refused the commit: something in it looks like a secret. Nothing
/// was applied; see [`last_blocked`] for where.
pub const BLOCKED: c_int = ffi::SQLITE_IOERR | (203 << 8);

/// Whether `error` is a commit refused by GitHub's push protection ([`BLOCKED`]).
pub fn is_blocked(error: &rusqlite::Error) -> bool {
    extended_code(error) == Some(BLOCKED)
}

/// Where GitHub's push protection found something that looks like a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    /// Each blocked place: a table and rows, an index, the changelog.
    pub places: Vec<String>,
    /// GitHub's message: the kind of secret, and a link to allow it.
    pub message: String,
}

/// What blocked the connection's last refused commit, if push protection did.
pub fn last_blocked(conn: &Connection) -> Option<Blocked> {
    vfs::last_blocked(conn)
}

/// What a connection's last commit did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The head after the commit: the new commit, or the unchanged head if there was nothing
    /// to publish.
    pub head: ObjectId,
    /// Whether a new git commit was published.
    pub published: bool,
}

/// Hand the connection's next commit a changelog: what the layer above recorded about the
/// transaction, stored in the commit's `changelog` blob. The commit takes it, so set it again
/// before running a transaction again; `None` clears it.
pub fn set_changelog(conn: &Connection, changelog: Option<Vec<u8>>) -> rusqlite::Result<()> {
    vfs::set_changelog(conn, changelog)
}

/// What the connection's last commit did, if it committed anything since it opened.
pub fn last_commit(conn: &Connection) -> Option<Committed> {
    vfs::last_commit(conn)
}

/// Run `f` with the time SQLite sees on this thread fixed at `unix_millis` (milliseconds since
/// the Unix epoch): `datetime('now')`, `CURRENT_TIMESTAMP` and the like. SQLite asks the VFS for
/// the time on the thread that runs the statement, so fixing it per thread fixes it per
/// statement; a transaction that runs every statement this way sees one time throughout, and so
/// does a replay of it.
pub fn with_clock<R>(unix_millis: i64, f: impl FnOnce() -> R) -> R {
    vfs::with_clock(unix_millis, f)
}

/// Whether `sql` ends with a complete SQL statement (SQLite's `sqlite3_complete`), so that a
/// semicolon inside a trigger body does not count as the end.
pub fn is_complete(sql: &str) -> bool {
    vfs::is_complete(sql)
}

fn extended_code(error: &rusqlite::Error) -> Option<c_int> {
    match error {
        rusqlite::Error::SqliteFailure(e, _) => Some(e.extended_code),
        _ => None,
    }
}

/// Whether `error` is a refused commit ([`CONFLICT`]): safe to run the transaction again.
pub fn is_conflict(error: &rusqlite::Error) -> bool {
    extended_code(error) == Some(CONFLICT)
}

/// Whether `error` is a commit of unknown outcome ([`OUTCOME_UNKNOWN`]).
pub fn is_outcome_unknown(error: &rusqlite::Error) -> bool {
    extended_code(error) == Some(OUTCOME_UNKNOWN)
}

/// Counters for one database.
#[derive(Default)]
pub struct Stats {
    pub transactions: AtomicU64,
    /// Transactions that committed, including those with nothing to publish.
    pub commits: AtomicU64,
    /// Commits that published a new git commit.
    pub published: AtomicU64,
    pub rebases: AtomicU64,
    pub conflicts: AtomicU64,
    /// Commits whose push outcome was unknown, settled afterwards by reading the head.
    pub settled: AtomicU64,
    /// Commits whose outcome stayed unknown (`OUTCOME_UNKNOWN`).
    pub unknown_outcomes: AtomicU64,
    /// Commits that failed without applying anything (network or server errors).
    pub failed: AtomicU64,
    pub pages_read: AtomicU64,
    pub pages_written: AtomicU64,
    /// Commits where page 1 carried real changes and was stored.
    pub page1_stored: AtomicU64,
    /// Pages SQLite rewrote with the bytes they already had, left out of commits.
    pub unchanged_pages: AtomicU64,
    pub prefetch_batches: AtomicU64,
    pub prefetched_pages: AtomicU64,
    pub catalog_updates: AtomicU64,
    /// Schema changes whose catalog blob could not be built (the pages are still committed).
    pub catalog_errors: AtomicU64,
}

impl Stats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn add(counter: &AtomicU64, n: usize) {
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// The page-store error behind the last I/O error SQLite reported on this thread (SQLite
/// itself says only "disk I/O error"). Taking it clears it.
pub fn take_last_error() -> Option<octopage_pagestore::Error> {
    file::take_last_error()
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Read the head ref at the start of every transaction (one round trip), so a connection
    /// sees other clients' commits promptly. Without it, it sees them after a conflict or poll.
    pub refresh_on_begin: bool,
    /// The most pages a scan fetches ahead in one batch; 0 turns read-ahead off.
    pub read_ahead: usize,
    /// The most pages the database may have (its live-size limit): a write that would grow it
    /// further fails with `SQLITE_FULL`, and `PRAGMA max_page_count` cannot raise it.
    pub max_pages: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            refresh_on_begin: true,
            read_ahead: 256,
            max_pages: None,
        }
    }
}

/// A database on a page store. Open SQLite connections to it with [`Database::connect`].
pub struct Database<T: Transport + 'static> {
    store: PageStore<T>,
    /// SQLite's page size: the page store's.
    page_size: usize,
    runtime: Handle,
    at: Option<ObjectId>,
    options: Options,
    counter: AtomicU32,
    pub stats: Stats,
}

impl<T: Transport + 'static> Database<T> {
    pub fn new(store: PageStore<T>, runtime: Handle) -> Arc<Self> {
        Self::with_options(store, runtime, Options::default())
    }

    /// Like [`Database::new`], without reading the head at the start of each transaction.
    pub fn without_refresh(store: PageStore<T>, runtime: Handle) -> Arc<Self> {
        let options = Options {
            refresh_on_begin: false,
            ..Options::default()
        };
        Self::with_options(store, runtime, options)
    }

    pub fn with_options(store: PageStore<T>, runtime: Handle, options: Options) -> Arc<Self> {
        Arc::new(Database {
            page_size: store.page_size(),
            store,
            runtime,
            at: None,
            options,
            counter: AtomicU32::new(0),
            stats: Stats::default(),
        })
    }

    /// The database as of `commit`, read-only (time travel).
    pub fn at(store: PageStore<T>, runtime: Handle, commit: ObjectId) -> Arc<Self> {
        let options = Options {
            refresh_on_begin: false,
            ..Options::default()
        };
        Arc::new(Database {
            page_size: store.page_size(),
            store,
            runtime,
            at: Some(commit),
            options,
            counter: AtomicU32::new(0),
            stats: Stats::default(),
        })
    }

    pub fn store(&self) -> &PageStore<T> {
        &self.store
    }

    /// SQLite's page size, which is the page store's.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    /// The runtime the VFS blocks on.
    pub fn runtime(&self) -> &Handle {
        &self.runtime
    }

    /// The commit this database is pinned to, for a read-only past view (see [`Database::at`]).
    pub fn pinned(&self) -> Option<ObjectId> {
        self.at
    }

    /// The catalog at the known head: `sqlite_schema` as of the last schema change.
    pub async fn catalog(&self) -> octopage_pagestore::Result<Option<Catalog>> {
        let snapshot = match self.at {
            Some(commit) => self.store.snapshot(commit).await?,
            None => self.store.latest().await?,
        };
        Ok(snapshot
            .catalog()
            .await?
            .and_then(|bytes| Catalog::parse(&bytes).ok()))
    }

    /// Open a SQLite connection. Call it (and use the connection) off the async runtime.
    pub fn connect(self: &Arc<Self>) -> rusqlite::Result<Connection> {
        let name = vfs::register(self.clone());
        let flags = match self.at {
            Some(_) => OpenFlags::SQLITE_OPEN_READ_ONLY,
            None => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        };
        let conn = Connection::open_with_flags_and_vfs(
            &name,
            flags | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            vfs::name::<T>(),
        )?;
        // Atomicity and durability come from the OctoPage commit; SQLite's rollback journal
        // stays in memory. synchronous=FULL makes SQLite call xSync, which is the commit point.
        // The VFS refuses later attempts to change these (see `file::check_pragma`).
        conn.execute_batch(
            "PRAGMA journal_mode=MEMORY; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY;",
        )?;
        if self.at.is_none()
            && conn.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))? == 0
        {
            // A new database: the page store's pages, with room for OctoPage's page header.
            conn.pragma_update(None, "page_size", self.page_size as i64)?;
            vfs::reserve_bytes(&conn, RESERVED)?;
        }
        if let (None, Some(max)) = (self.at, self.options.max_pages) {
            conn.pragma_update(None, "max_page_count", max)?;
        }
        Ok(conn)
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}
