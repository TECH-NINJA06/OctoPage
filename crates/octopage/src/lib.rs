#![deny(missing_docs)]

use std::sync::Arc;

use octopage_git::{Credentials, HttpConfig, StaticToken};
use octopage_pagestore::PageStore;
use tokio::runtime::Handle;

mod asof;
pub mod changelog;
mod conn;
mod error;
mod settings;
mod signing;
mod sql;
mod value;
pub mod webhook;

pub use changelog::{Changelog, Statement};
pub use conn::{Connection, Outcome, Rows, Transaction};
pub use error::{Error, Result};
pub use octopage_git::{InMemory, ObjectId, SmartHttp, Transport};
pub use octopage_pagestore::CommitSigner;
pub use octopage_pagestore::{Encryption, Kdf, KeyProvider, Nonces, RecoveryKey, Unlock};
pub use octopage_pagestore::{Moved, Scan};
pub use octopage_sqlite::{Catalog, Committed, SchemaObject, Stats};
pub use settings::{Retention, Settings};
pub use signing::SshSigner;
pub use value::Value;
pub use webhook::{PushEvent, verify_signature};

/// The names most programs need: `use octopage::prelude::*;`.
pub mod prelude {
    pub use crate::{
        Config, Connection, Database, Error, Result, Retention, Rows, Settings, Transaction, Value,
        params,
    };
}

/// How to open or create a database. Start from `Config::default()` and change fields.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    /// The page store: branch, caches, read batching, head polling, and `unlock` for an
    /// encrypted database.
    pub store: octopage_pagestore::Config,
    /// The SQLite VFS: reading the head at each transaction, read-ahead.
    pub sqlite: octopage_sqlite::Options,
    /// Attempts for a transaction whose commits keep being refused, before
    /// [`Error::Serialization`].
    pub max_attempts: u32,
    /// The database is meant to be public (an open dataset): don't warn that anyone can read
    /// an unencrypted database in a public repository.
    pub public: bool,
    /// Refuse to create or open an encrypted database in a public repository, where anyone can
    /// copy it and try passphrases offline, instead of only warning.
    pub require_private: bool,
    /// The page size of a new database: 4096 (the default), 8192 or 16384 bytes. Larger pages
    /// mean fewer blobs and round trips for scans, and more bytes per small change.
    pub page_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            store: octopage_pagestore::Config::default(),
            sqlite: octopage_sqlite::Options::default(),
            max_attempts: 8,
            public: false,
            require_private: false,
            page_size: octopage_sqlite::PAGE_SIZE,
        }
    }
}

/// A database: one branch of one repository. Cheap to clone.
pub struct Database<T: Transport + 'static = SmartHttp> {
    sqlite: Arc<octopage_sqlite::Database<T>>,
    config: Arc<Config>,
    warnings: Arc<Vec<String>>,
}

impl<T: Transport + 'static> Clone for Database<T> {
    fn clone(&self) -> Self {
        Database {
            sqlite: self.sqlite.clone(),
            config: self.config.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

/// One commit in the database's history.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LogEntry {
    /// The commit.
    pub commit: ObjectId,
    /// Its parent (none for the first commit).
    pub parent: Option<ObjectId>,
    /// Commit time, in seconds since the Unix epoch.
    pub time: i64,
    /// The commit message: `octopage txn <id>` for a transaction.
    pub message: String,
    /// For a commit copied into a new generation: the commit it was copied from.
    pub rewritten_from: Option<ObjectId>,
    /// What the commit's transaction ran; empty for commits that recorded nothing.
    pub changelog: Changelog,
}

impl<T: Transport + 'static> Database<T> {
    /// Create a new database on `config.store.branch`. Call it on the async runtime.
    pub async fn create(transport: T, config: Config) -> Result<Self> {
        let warnings = check_visibility(&transport, false, &config).await?;
        let store = PageStore::create(transport, config.store.clone(), config.page_size).await?;
        Self::with_warnings(store, config, warnings).await
    }

    /// Create a new encrypted database: pages, catalog and changelogs are compressed and
    /// encrypted, and the key is wrapped for `encryption.key` and for a recovery key, which is
    /// returned here and nowhere else. Store it: it opens the database if the passphrase or key
    /// provider is lost.
    pub async fn create_encrypted(
        transport: T,
        config: Config,
        encryption: Encryption,
    ) -> Result<(Self, RecoveryKey)> {
        let warnings = check_visibility(&transport, true, &config).await?;
        let (store, recovery) = PageStore::create_encrypted(
            transport,
            config.store.clone(),
            config.page_size,
            encryption,
        )
        .await?;
        Ok((
            Self::with_warnings(store, config, warnings).await?,
            recovery,
        ))
    }

    /// Open the database on `config.store.branch`; an encrypted one needs
    /// `config.store.unlock`. Call it on the async runtime.
    pub async fn open(transport: T, config: Config) -> Result<Self> {
        let store = PageStore::open(transport, config.store.clone()).await?;
        let warnings = check_visibility(&store.transport(), store.is_encrypted(), &config).await?;
        Self::with_warnings(store, config, warnings).await
    }

    /// The database on an open store, with its settings applied (the live-size limit, unless
    /// `config.sqlite.max_pages` sets one).
    async fn with_warnings(
        store: PageStore<T>,
        mut config: Config,
        warnings: Vec<String>,
    ) -> Result<Self> {
        if config.sqlite.max_pages.is_none() {
            let settings = read_settings(&store.latest().await?).await?;
            config.sqlite.max_pages = Some(settings.max_pages(store.page_size()));
        }
        let mut db = Self::from_store(store, config)?;
        db.warnings = Arc::new(warnings);
        Ok(db)
    }

    /// A database on a page store that is already open. Call it on the async runtime, whose
    /// handle the database keeps for SQLite's calls into the page store.
    pub fn from_store(store: PageStore<T>, config: Config) -> Result<Self> {
        let runtime = Handle::try_current().map_err(|_| {
            Error::Invalid("OctoPage needs a Tokio runtime to open a database".into())
        })?;
        Ok(Database {
            sqlite: octopage_sqlite::Database::with_options(store, runtime, config.sqlite.clone()),
            config: Arc::new(config),
            warnings: Arc::default(),
        })
    }

    /// Whether pages, catalog and changelogs are encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.store().is_encrypted()
    }

    /// Things worth telling the user about this database, such as that its repository is public.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Open a connection. Use it off the async runtime.
    pub fn connect(&self) -> Result<Connection<T>> {
        Connection::open(self.clone())
    }

    /// Run `body` in a transaction on a new connection (see [`Connection::transaction`]).
    pub fn transaction<R>(&self, body: impl FnMut(&Transaction<'_, T>) -> Result<R>) -> Result<R> {
        self.connect()?.transaction(body)
    }

    /// The configuration it was opened with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The page store underneath: snapshots, history, caches, the writer lease.
    pub fn store(&self) -> &PageStore<T> {
        self.sqlite.store()
    }

    /// Counters: transactions, pages read and written, commits, rebases, conflicts.
    pub fn stats(&self) -> &Stats {
        &self.sqlite.stats
    }

    /// The newest head this client knows of.
    pub fn head(&self) -> ObjectId {
        self.store().head()
    }

    /// Read the head ref now.
    pub async fn refresh(&self) -> Result<ObjectId> {
        Ok(self.store().refresh().await?)
    }

    /// Where the database lives now: `OWNER/NAME` on GitHub. It changes when the database moves
    /// to a new generation (see [`Database::moves`]); point new clients there.
    pub fn location(&self) -> Option<String> {
        self.store().location()
    }

    /// How many times this client followed the database to a new repository.
    pub fn moves(&self) -> u64 {
        self.store().moves()
    }

    /// The maintenance settings at the known head.
    pub async fn settings(&self) -> Result<Settings> {
        read_settings(&self.store().latest().await?).await
    }

    /// Change the maintenance settings: a commit of its own. The live-size limit applies to
    /// clients that open the database from then on. Returns the commit.
    pub async fn set_settings(&self, settings: &Settings) -> Result<ObjectId> {
        settings.validate().map_err(Error::Invalid)?;
        self.commit_beside(Beside::Settings(settings.to_bytes()))
            .await
    }

    /// Write a file into the commit tree beside the database (a GitHub Actions workflow under
    /// `.github/workflows/`, say): a commit of its own, and the file stays in every commit after
    /// it. `None` removes it. Returns the commit.
    pub async fn put_file(&self, path: &str, contents: Option<bytes::Bytes>) -> Result<ObjectId> {
        self.commit_beside(Beside::File(path.to_string(), contents))
            .await
    }

    /// Commit a change that touches no pages, starting again from the new head whenever
    /// another client commits first.
    async fn commit_beside(&self, change: Beside) -> Result<ObjectId> {
        for _ in 0..self.config.max_attempts.max(1) {
            let mut txn = self.store().begin().await?;
            match &change {
                Beside::Settings(bytes) => txn.set_settings(bytes.clone()),
                Beside::File(path, contents) => txn.put_file(path, contents.clone()).await?,
            }
            match txn.commit().await? {
                octopage_pagestore::Outcome::Committed(c) => return Ok(c.head),
                octopage_pagestore::Outcome::Conflict(_) => {
                    self.store().refresh().await?;
                }
            }
        }
        Err(Error::Serialization {
            attempts: self.config.max_attempts,
        })
    }

    /// Check every page of the database at `commit` (by default the head): see
    /// [`octopage_pagestore::Snapshot::scan`].
    pub async fn scan(&self, commit: Option<ObjectId>) -> Result<Scan> {
        let snapshot = match commit {
            Some(commit) => self.store().snapshot(commit).await?,
            None => self.store().latest().await?,
        };
        Ok(snapshot.scan().await?)
    }

    /// The commit a read-only past view is pinned to (see [`Database::at`]).
    pub fn pinned(&self) -> Option<ObjectId> {
        self.sqlite.pinned()
    }

    /// The database as of `commit`, read-only. It shares this database's caches.
    pub fn at(&self, commit: ObjectId) -> Database<T> {
        Database {
            sqlite: octopage_sqlite::Database::at(
                self.store().clone(),
                self.sqlite.runtime().clone(),
                commit,
            ),
            config: self.config.clone(),
            warnings: self.warnings.clone(),
        }
    }

    /// The commit an `AS OF` target names: a commit id prefix (at least 4 hex digits) or a UTC
    /// time (`2026-09-01`, `2026-09-01 14:30:00`), which means the newest commit made then or
    /// before. Reads the head first.
    pub async fn resolve(&self, target: &str) -> Result<ObjectId> {
        let fail = |reason: String| Error::AsOf {
            target: target.to_string(),
            reason,
        };
        let parsed = asof::parse(target).map_err(fail)?;
        self.store().refresh().await?;
        // A time is found in the newest commits; widen the search only if needed. A commit id
        // prefix must be checked against the whole history, to catch ambiguity.
        let mut limit = match parsed {
            asof::Target::Time(_) => 64,
            asof::Target::Commit(_) => usize::MAX,
        };
        loop {
            let history = self.store().history(limit).await?;
            match asof::find(&history, &parsed) {
                Ok(commit) => return Ok(commit),
                Err(reason) if history.len() < limit => return Err(fail(reason)),
                Err(_) => limit = limit.saturating_mul(8),
            }
        }
    }

    /// The most recent `limit` commits from the known head, newest first, with the statements
    /// each ran.
    pub async fn log(&self, limit: usize) -> Result<Vec<LogEntry>> {
        Ok(self
            .store()
            .log(limit)
            .await?
            .into_iter()
            .map(|e| LogEntry {
                commit: e.commit,
                parent: e.parent,
                time: e.time,
                message: e.message,
                rewritten_from: e.rewritten_from,
                changelog: changelog::decode(&e.changelog).unwrap_or_default(),
            })
            .collect())
    }

    /// Take (or renew) the advisory writer lease: while it lasts, other cooperating clients
    /// wait before committing instead of racing this one. For a steady writer; release it when
    /// done (it also expires, after `config.store.lease_ttl`). Returns whether this client holds
    /// it. Clients take the lease by themselves after losing several races in a row.
    pub async fn take_lease(&self) -> Result<bool> {
        Ok(self.store().acquire_lease().await?)
    }

    /// Give up the writer lease, if this client holds it.
    pub async fn release_lease(&self) -> Result<()> {
        Ok(self.store().release_lease().await?)
    }

    /// This database's branch: its ref without `refs/heads/`.
    pub fn branch_name(&self) -> &str {
        let branch = &self.config.store.branch;
        branch.strip_prefix(HEADS).unwrap_or(branch)
    }

    /// The repository's branches (each a database), with their heads.
    pub async fn branches(&self) -> Result<Vec<(String, ObjectId)>> {
        let refs = self.store().transport().list_refs(&[HEADS]).await?;
        Ok(refs
            .into_iter()
            .filter_map(|r| {
                r.name
                    .strip_prefix(HEADS)
                    .map(|name| (name.to_string(), r.id))
            })
            .collect())
    }

    /// Create branch `name` at the head of branch `from` (by default this one): a new database
    /// that shares this one's history, and its keys if it is encrypted. Returns its head.
    pub async fn create_branch(&self, name: &str, from: Option<&str>) -> Result<ObjectId> {
        let target = branch_ref(name)?;
        let head = match from {
            None => self.refresh().await?,
            Some(from) => self.branch_head(from).await?,
        };
        let created = self
            .store()
            .transport()
            .push(&[octopage_git::RefUpdate::create(&target, head)], &[])
            .await;
        match created {
            Ok(()) => Ok(head),
            Err(octopage_git::Error::Conflict { .. }) => {
                Err(Error::Invalid(format!("branch {name} already exists")))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn branch_head(&self, name: &str) -> Result<ObjectId> {
        let target = branch_ref(name)?;
        self.store()
            .transport()
            .list_refs(&[&target])
            .await?
            .into_iter()
            .find(|r| r.name == target)
            .map(|r| r.id)
            .ok_or_else(|| Error::Invalid(format!("there is no branch {name}")))
    }

    /// Delete branch `name`. Its commits stay reachable only from other branches that share them.
    pub async fn drop_branch(&self, name: &str) -> Result<()> {
        let target = branch_ref(name)?;
        if target == self.config.store.branch {
            return Err(Error::Invalid(
                "a database cannot drop its own branch".into(),
            ));
        }
        let head = self.branch_head(name).await?;
        self.store()
            .transport()
            .push(&[octopage_git::RefUpdate::delete(&target, head)], &[])
            .await?;
        Ok(())
    }

    /// Another branch of this repository, over the same transport and with the same settings
    /// (including how to unlock it).
    pub async fn open_branch(&self, name: &str) -> Result<Database<T>> {
        let mut config = (*self.config).clone();
        config.store.branch = branch_ref(name)?;
        let store =
            PageStore::open_shared(self.store().shared_transport(), config.store.clone()).await?;
        let warnings = self.warnings.to_vec(); // the same repository
        Self::with_warnings(store, config, warnings).await
    }

    /// `sqlite_schema` as of the last schema change, from the catalog blob.
    pub async fn catalog(&self) -> Result<Option<Catalog>> {
        Ok(self.sqlite.catalog().await?)
    }
}

const HEADS: &str = "refs/heads/";

/// A change beside the database's pages.
enum Beside {
    Settings(Vec<u8>),
    File(String, Option<bytes::Bytes>),
}

/// The settings stored in `snapshot`, or the defaults.
pub(crate) async fn read_settings<T: Transport + 'static>(
    snapshot: &octopage_pagestore::Snapshot<T>,
) -> Result<Settings> {
    match snapshot.settings().await? {
        Some(bytes) => Settings::parse(&bytes).map_err(Error::Invalid),
        None => Ok(Settings::default()),
    }
}

/// `refs/heads/<name>`, for a name git accepts as a branch.
pub(crate) fn branch_ref(name: &str) -> Result<String> {
    let valid = !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
    if !valid {
        return Err(Error::Invalid(format!(
            "{name:?} is not a valid branch name"
        )));
    }
    Ok(format!("{HEADS}{name}"))
}

/// What a merge did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MergeReport {
    /// The other branch's commits replayed here, each with the commit it made (the unchanged
    /// head if it changed nothing).
    pub merged: Vec<(ObjectId, ObjectId)>,
    /// The other branch's commits left out: merged before, merged in from here, or with
    /// nothing to replay.
    pub skipped: usize,
}

/// The commits of `source` to replay on `target`, oldest first.
pub(crate) struct MergePlan {
    pub replay: Vec<(ObjectId, Changelog)>,
    pub skipped: usize,
}

/// Plan a merge: the source's commits since the two branches diverged, minus those whose
/// changes the target already has (merged before, or merged into the source from the target).
/// Branches share history only up to where they forked; merges replay statements, so they
/// never share commits after that.
pub(crate) async fn plan_merge<T: Transport + 'static, U: Transport + 'static>(
    target: &Database<T>,
    source: &Database<U>,
) -> Result<MergePlan> {
    target.refresh().await?;
    source.refresh().await?;
    let target_history = target.store().history(usize::MAX).await?;
    // A commit copied into a new generation also stands for the one it was copied from.
    let target_ids: std::collections::HashSet<ObjectId> = target_history
        .iter()
        .flat_map(|c| std::iter::once(c.commit).chain(c.rewritten_from))
        .collect();
    let source_history = source.store().history(usize::MAX).await?;
    let fork = source_history
        .iter()
        .position(|c| {
            target_ids.contains(&c.commit)
                || c.rewritten_from.is_some_and(|o| target_ids.contains(&o))
        })
        .ok_or_else(|| Error::Invalid("the two databases share no history".into()))?;
    let fork_id = source_history[fork].commit;
    let target_since = target_history
        .iter()
        .position(|c| c.commit == fork_id || c.rewritten_from == Some(fork_id))
        .expect("the fork is in the target's history");
    // What the target already took from elsewhere, by origin.
    let merged: std::collections::HashSet<ObjectId> = target
        .log(target_since)
        .await?
        .into_iter()
        .filter_map(|e| e.changelog.origin)
        .collect();
    let mut plan = MergePlan {
        replay: Vec::new(),
        skipped: 0,
    };
    for entry in source.log(fork).await?.into_iter().rev() {
        let have = |id: &ObjectId| target_ids.contains(id) || merged.contains(id);
        if have(&entry.commit)
            || entry.rewritten_from.as_ref().is_some_and(have)
            || entry.changelog.origin.as_ref().is_some_and(have)
            || entry.changelog.statements.is_empty()
        {
            plan.skipped += 1;
        } else {
            plan.replay.push((entry.commit, entry.changelog));
        }
    }
    Ok(plan)
}

/// Refuse, or warn about, a database in a repository anyone can read.
async fn check_visibility<T: Transport>(
    transport: &T,
    encrypted: bool,
    config: &Config,
) -> Result<Vec<String>> {
    let public = match transport.is_public().await {
        Ok(public) => public,
        Err(error) => {
            tracing::debug!(%error, "could not tell whether the repository is public");
            None
        }
    };
    if public != Some(true) {
        return Ok(Vec::new());
    }
    let warning = if encrypted {
        if config.require_private {
            return Err(Error::PublicRepository);
        }
        "the repository is public: anyone can copy this encrypted database and try passphrases \
         offline, so use a long passphrase or a key provider"
    } else if config.public {
        return Ok(Vec::new()); // meant to be public
    } else {
        "the repository is public: anyone can read this database (declare it public to silence \
         this, or create it encrypted)"
    };
    tracing::warn!("{warning}");
    Ok(vec![warning.to_string()])
}

/// The statements of a batch, each with its semicolon (a trigger body stays whole).
pub fn statements(sql: &str) -> Vec<&str> {
    sql::split_statements(sql)
}

/// Whether `sql` ends a complete statement, for reading SQL a line at a time.
pub fn is_complete(sql: &str) -> bool {
    octopage_sqlite::is_complete(sql)
}

/// A transport to the GitHub repository `OWNER/NAME`, signing in with `token` if given: a
/// fine-grained personal access token (signs in as the owner) or an app installation token
/// (`ghs_…`, signs in as `x-access-token`).
pub fn github(repo: &str, token: Option<&str>) -> Result<SmartHttp> {
    let owner = repo.split('/').next().unwrap_or_default();
    remote(&format!("https://github.com/{repo}.git"), token, owner)
}

/// A transport to any smart-HTTP git remote, signing in as `user` with `token` if given.
pub fn remote(url: &str, token: Option<&str>, user: &str) -> Result<SmartHttp> {
    let creds = token.map(|token| {
        let user = if token.starts_with("ghs_") {
            "x-access-token"
        } else {
            user
        };
        Credentials::git(
            user.to_string(),
            Arc::new(StaticToken::new(token.to_string())),
        )
    });
    Ok(SmartHttp::new(url, creds, HttpConfig::default())?)
}
