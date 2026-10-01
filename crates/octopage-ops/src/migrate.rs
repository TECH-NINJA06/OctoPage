use std::sync::Arc;
use std::time::Duration;

use octopage::{Database, Error, ObjectId, Result, Settings, Transport};
use octopage_git::{EntryMode, Tree};
use octopage_pagestore::{GEN_PREFIX, PageStore, Root};
use rusqlite::types::Value as SqlValue;

use crate::host::Host;
use crate::records::{
    self, GenerationRecord, RefLease, WRITER_LEASE_REF, next_location, take_lease,
};
use crate::rollover::{Copy, check_checkpoints, seal};
use crate::walk::{Commits, database_root, root_tree};

#[derive(Clone, Debug)]
pub struct MigrateOptions {
    /// The new page size: 4096, 8192 or 16384.
    pub page_size: usize,
    /// The new repository; by default `<name>-g<n+1>`.
    pub location: Option<String>,
    /// Rows per commit while copying.
    pub batch_rows: usize,
    /// How long cooperating writers pause while the job seals the old repository.
    pub seal_lease: Duration,
    pub max_rounds: u32,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        MigrateOptions {
            page_size: 8192,
            location: None,
            batch_rows: 2000,
            seal_lease: Duration::from_secs(15),
            max_rounds: 30,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MigrateReport {
    pub from: String,
    pub to: String,
    pub generation: u64,
    pub page_size: usize,
    pub tables: usize,
    pub rows: u64,
    /// Commits that landed during the copy and were run again in the new repository.
    pub replayed: usize,
    pub rounds: u32,
}

/// Move the database `db` (opened with its key, if it is encrypted) to a new repository with
/// pages of `options.page_size` bytes. See the module docs.
pub async fn migrate<T: Transport + 'static, H: Host>(
    db: &Database<T>,
    host: &H,
    options: &MigrateOptions,
) -> Result<MigrateReport> {
    let source = db.store().transport();
    let from = source
        .location()
        .ok_or_else(|| Error::Invalid("the transport cannot say where its repository is".into()))?;
    let branch = db.config().store.branch.clone();
    if !octopage_pagestore::page::PAGE_SIZES.contains(&options.page_size) {
        return Err(Error::Invalid(format!(
            "pages are 4096, 8192 or 16384 bytes, not {}",
            options.page_size
        )));
    }
    if db.store().page_size() == options.page_size {
        return Err(Error::Invalid(format!(
            "the database's pages are {} bytes already",
            options.page_size
        )));
    }
    let all = source.list_refs(&[]).await?;
    if all.iter().any(|r| r.name.starts_with(GEN_PREFIX)) {
        return Err(Error::Invalid(format!(
            "{from} has moved to a new generation already"
        )));
    }
    let mut commits = Commits::default();
    check_checkpoints(&*source, &all, &mut commits).await?;
    // Other databases would be left behind: migrate is for a repository of one.
    let mut others = Vec::new();
    for head in all
        .iter()
        .filter(|r| r.name.starts_with("refs/heads/") && r.name != branch)
    {
        let (_, commit) = commits.load(&*source, head.id).await?;
        if database_root(&root_tree(&*source, commit.tree).await?).is_some() {
            return Err(Error::Invalid(format!(
                "{} holds another database: migrating moves the whole repository, so roll the \
                 others over into a repository of their own first",
                head.name
            )));
        }
        others.push(head.clone());
    }
    let generation = GenerationRecord::read(&*source)
        .await?
        .map_or(1, |(_, r)| r.generation);
    let next = generation + 1;
    let to = options
        .location
        .clone()
        .unwrap_or_else(|| next_location(&from, next));
    let public = source.is_public().await.ok().flatten().unwrap_or(false);
    if host.exists(&to).await? {
        return Err(Error::Invalid(format!("{to} exists already")));
    }
    host.create_repository(&to, public).await?;
    let target = Arc::new((*source).relocate(&to)?);
    GenerationRecord {
        generation: next,
        previous: Some(from.clone()),
        created: records::now(),
        sealed: None,
        previous_deleted: false,
    }
    .write(&*target, None)
    .await?;
    tracing::info!(%from, %to, page_size = options.page_size, "migrating");

    // The new database: the same keys, other pages.
    let config = octopage_pagestore::Config {
        branch: branch.clone(),
        head_poll: None,
        ..db.config().store.clone()
    };
    let store = PageStore::create_like(
        (*target).relocate(&to)?,
        config,
        options.page_size,
        db.store(),
    )
    .await?;
    let copy_db = Database::from_store(store, octopage::Config::default())?;

    let head = db.refresh().await?;
    let (tables, rows) = copy_rows(db, &copy_db, head, options.batch_rows).await?;
    copy_beside(db, &copy_db, head).await?;
    let mut copied = head;
    let mut replayed = 0;

    let mut exact = Copy::new(
        source.clone(),
        target.clone(),
        records::now(),
        32 << 20,
        Commits::default(),
    )
    .await?;
    let holder = format!("octopage-maintenance-{:016x}", fastrand::u64(..));
    let mut lease: Option<RefLease> = None;
    let mut rounds = 0;
    let sealed = loop {
        rounds += 1;
        if rounds > options.max_rounds {
            break Err(Error::Invalid(format!(
                "writers kept committing: gave up sealing {from} after {} rounds",
                options.max_rounds
            )));
        }
        let head = db.refresh().await?;
        if head != copied {
            match replay_since(db, &copy_db, copied, head).await {
                Ok(n) => replayed += n,
                Err(e) => break Err(e),
            }
            copy_beside(db, &copy_db, head).await?;
            copied = head;
        }
        let refs: Vec<_> = source
            .list_refs(&["refs/heads/", "refs/tags/"])
            .await?
            .into_iter()
            .filter(|r| others.iter().any(|o| o.name == r.name) || r.name.starts_with("refs/tags/"))
            .collect();
        if let Err(e) = exact.sync(&refs).await {
            break Err(e);
        }
        if rounds == 1 {
            lease = take_lease(
                &*source,
                WRITER_LEASE_REF,
                &holder,
                options.seal_lease,
                options.seal_lease,
            )
            .await?;
            continue;
        }
        let databases = [(branch.clone(), copied)];
        match seal(&*source, &mut commits, &databases, &to, next).await {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(e) => break Err(e),
        }
    };
    if let Some(lease) = lease {
        lease.release(&*source).await;
    }
    sealed?;
    exact.finish().await?;
    if let Some((commit, mut record)) = GenerationRecord::read(&*target).await? {
        record.sealed = Some(records::now());
        record.write(&*target, Some(commit)).await?;
    }
    Ok(MigrateReport {
        from,
        to,
        generation: next,
        page_size: options.page_size,
        tables,
        rows,
        replayed,
        rounds,
    })
}

/// Copy the schema and every row of `from` at `commit` into the empty database `to`.
async fn copy_rows<T: Transport + 'static>(
    from: &Database<T>,
    to: &Database<T>,
    commit: ObjectId,
    batch: usize,
) -> Result<(usize, u64)> {
    let source = octopage_sqlite::Database::at(
        from.store().clone(),
        tokio::runtime::Handle::current(),
        commit,
    );
    let target = octopage_sqlite::Database::with_options(
        to.store().clone(),
        tokio::runtime::Handle::current(),
        octopage_sqlite::Options::default(),
    );
    tokio::task::spawn_blocking(move || -> Result<(usize, u64)> {
        let src = source.connect()?;
        let dst = target.connect()?;
        Ok(copy_sql(&src, &dst, batch)?)
    })
    .await
    .map_err(|e| Error::Invalid(format!("the copy stopped: {e}")))?
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Copy a SQLite database's schema and rows, `batch` rows a transaction. Returns the tables
/// and rows copied.
fn copy_sql(
    src: &rusqlite::Connection,
    dst: &rusqlite::Connection,
    batch: usize,
) -> rusqlite::Result<(usize, u64)> {
    let schema: Vec<(String, String, String)> = src
        .prepare(
            "SELECT type, name, sql FROM sqlite_schema \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let tables: Vec<&(String, String, String)> =
        schema.iter().filter(|(kind, ..)| kind == "table").collect();
    for (_, _, sql) in &tables {
        dst.execute_batch(sql)?;
    }
    let mut rows = 0u64;
    for (_, name, _) in &tables {
        let columns: Vec<String> = src
            .prepare("SELECT name FROM pragma_table_xinfo(?1) WHERE hidden = 0")?
            .query_map([name], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let with_rowid = src
            .prepare(&format!("SELECT rowid FROM {} LIMIT 0", ident(name)))
            .is_ok();
        let mut names: Vec<String> = columns.iter().map(|c| ident(c)).collect();
        if with_rowid {
            names.insert(0, "rowid".into());
        }
        let list = names.join(", ");
        let marks = vec!["?"; names.len()].join(", ");
        let mut select = src.prepare(&format!("SELECT {list} FROM {}", ident(name)))?;
        let insert = format!("INSERT INTO {}({list}) VALUES ({marks})", ident(name));
        let mut stream = select.query([])?;
        let mut in_batch = 0;
        dst.execute_batch("BEGIN")?;
        while let Some(row) = stream.next()? {
            let values: Vec<SqlValue> = (0..names.len())
                .map(|i| row.get::<_, SqlValue>(i))
                .collect::<rusqlite::Result<_>>()?;
            dst.execute(&insert, rusqlite::params_from_iter(values))?;
            rows += 1;
            in_batch += 1;
            if in_batch == batch {
                dst.execute_batch("COMMIT; BEGIN")?;
                in_batch = 0;
            }
        }
        dst.execute_batch("COMMIT")?;
    }
    // AUTOINCREMENT counters, then indexes, views and triggers (after the rows, so triggers do
    // not fire on the copy).
    if src.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name = 'sqlite_sequence'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0
    {
        let sequences: Vec<(String, i64)> = src
            .prepare("SELECT name, seq FROM sqlite_sequence")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        dst.execute_batch("BEGIN; DELETE FROM sqlite_sequence")?;
        for (name, seq) in sequences {
            dst.execute(
                "INSERT INTO sqlite_sequence(name, seq) VALUES(?1, ?2)",
                rusqlite::params![name, seq],
            )?;
        }
        dst.execute_batch("COMMIT")?;
    }
    for kind in ["index", "view", "trigger"] {
        for (_, _, sql) in schema.iter().filter(|(k, ..)| k == kind) {
            dst.execute_batch(sql)?;
        }
    }
    for pragma in ["user_version", "application_id"] {
        let value: i64 = src.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))?;
        if value != 0 {
            dst.execute_batch(&format!("PRAGMA {pragma} = {value}"))?;
        }
    }
    Ok((tables.len(), rows))
}

/// Run the commits after `since`, up to `head`, again in `to`, from their changelogs.
async fn replay_since<T: Transport + 'static>(
    from: &Database<T>,
    to: &Database<T>,
    since: ObjectId,
    head: ObjectId,
) -> Result<usize> {
    let transport = from.store().transport();
    let mut commits = Commits::default();
    let chain = commits.chain(&*transport, head, Some(since)).await?;
    let mut changelogs = Vec::new();
    for commit in chain.iter().rev() {
        let snapshot = from.store().snapshot(*commit).await?;
        let bytes = snapshot.changelog().await?;
        let changelog = octopage::changelog::decode(&bytes).unwrap_or_default();
        if changelog.statements.is_empty() {
            let parent = from.store().snapshot(snapshot.parents()[0]).await?;
            if !from.store().diff(&parent, &snapshot).await?.is_empty() {
                return Err(Error::Invalid(format!(
                    "commit {commit} changed pages without a changelog, so it cannot be run \
                     again: migrate again"
                )));
            }
            continue; // settings or files; copied separately
        }
        changelogs.push(changelog);
    }
    let to = to.clone();
    let count = changelogs.len();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = to.connect()?;
        for changelog in &changelogs {
            conn.replay(changelog)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| Error::Invalid(format!("the replay stopped: {e}")))??;
    Ok(count)
}

/// Copy the settings and the files beside the database (workflows, say) as they are at
/// `commit`.
async fn copy_beside<T: Transport + 'static>(
    from: &Database<T>,
    to: &Database<T>,
    commit: ObjectId,
) -> Result<()> {
    let snapshot = from.store().snapshot(commit).await?;
    if let Some(bytes) = snapshot.settings().await? {
        let settings = Settings::parse(&bytes).map_err(Error::Invalid)?;
        if to.settings().await? != settings {
            to.set_settings(&settings).await?;
        }
    }
    let transport = from.store().transport();
    let theirs = files(&*transport, snapshot.root()).await?;
    let ours = files(&*to.store().transport(), to.store().latest().await?.root()).await?;
    for (path, bytes) in &theirs {
        if ours.iter().all(|(p, b)| p != path || b != bytes) {
            to.put_file(path, Some(bytes.clone())).await?;
        }
    }
    for (path, _) in &ours {
        if theirs.iter().all(|(p, _)| p != path) {
            to.put_file(path, None).await?;
        }
    }
    Ok(())
}

/// Every file beside the database, with its path.
async fn files<T: Transport>(t: &T, root: &Root) -> Result<Vec<(String, bytes::Bytes)>> {
    let mut out = Vec::new();
    let mut stack: Vec<(String, octopage_git::TreeEntry)> = root
        .extra
        .iter()
        .map(|e| (String::from_utf8_lossy(&e.name).into_owned(), e.clone()))
        .collect();
    while let Some((path, entry)) = stack.pop() {
        let object = records::object(t, entry.id).await?;
        match entry.mode {
            EntryMode::Tree => {
                let tree = Tree::decode(object.data()).map_err(Error::from)?;
                for child in tree.entries() {
                    let name = String::from_utf8_lossy(&child.name);
                    stack.push((format!("{path}/{name}"), child.clone()));
                }
            }
            _ => out.push((path, object.data().clone())),
        }
    }
    Ok(out)
}
