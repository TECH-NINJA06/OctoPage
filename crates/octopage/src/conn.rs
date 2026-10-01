use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use octopage_git::{ObjectId, SmartHttp, Transport};
use octopage_sqlite::Committed;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;

use crate::changelog::{self, Changelog, Statement};
use crate::sql::{self, BranchCommand, Keyword};
use crate::{Database, Error, MergeReport, Result, Value};

/// A query's result: column names and rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    /// The column names, in order.
    pub columns: Vec<String>,
    /// The rows, each with one value per column.
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    /// How many rows there are.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The first column of the first row, or `Null`.
    pub fn value(&self) -> Value {
        self.rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_default()
    }
}

/// What one statement did.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Outcome {
    /// The rows it returned (none, for most statements that change data).
    pub rows: Rows,
    /// Rows inserted, updated or deleted (triggers included).
    pub changed: u64,
}

/// Past databases a connection keeps open for `AS OF` queries.
const PAST_CONNECTIONS: usize = 4;

/// A transaction's time (milliseconds since the Unix epoch) and random seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Clock {
    now: i64,
    seed: u64,
}

impl Clock {
    fn fresh() -> Self {
        Clock {
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            seed: fastrand::u64(..),
        }
    }
}

const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// SplitMix64's output function.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The next number from a SplitMix64 stream.
fn draw(state: &AtomicU64) -> u64 {
    mix(state
        .fetch_add(GOLDEN, Ordering::Relaxed)
        .wrapping_add(GOLDEN))
}

/// A connection to a database. Use it off the async runtime (for example in
/// `tokio::task::spawn_blocking` or a plain thread): SQLite calls into the page store
/// synchronously.
pub struct Connection<T: Transport + 'static = SmartHttp> {
    db: Database<T>,
    sqlite: rusqlite::Connection,
    /// Statements recorded in the open transaction, for its changelog.
    recorded: RefCell<Vec<Statement>>,
    /// The open transaction's clock.
    clock: Cell<Option<Clock>>,
    /// The clock the next transaction starts with (a re-run or a replay), instead of a fresh one.
    next_clock: Cell<Option<Clock>>,
    /// The random stream of the running statement (`random()`, `randomblob()`).
    random: Arc<AtomicU64>,
    /// While a merge replays another branch's commit: that commit, for the changelog.
    origin: Cell<Option<ObjectId>>,
    /// Read-only connections to past commits, for `AS OF` queries.
    past: RefCell<Vec<(ObjectId, rusqlite::Connection)>>,
}

/// An open transaction, inside [`Connection::transaction`].
pub struct Transaction<'c, T: Transport + 'static = SmartHttp> {
    conn: &'c Connection<T>,
}

fn backoff(attempt: u32) {
    let ceiling = (5u64 << attempt.min(7)).min(1_000);
    std::thread::sleep(Duration::from_millis(fastrand::u64(0..=ceiling)));
}

/// Replace SQLite's `random()` and `randomblob(N)` with ones that draw from `state`.
fn install_random(sqlite: &rusqlite::Connection, state: &Arc<AtomicU64>) -> rusqlite::Result<()> {
    let s = state.clone();
    sqlite.create_scalar_function("random", 0, FunctionFlags::SQLITE_UTF8, move |_| {
        Ok(draw(&s) as i64)
    })?;
    let s = state.clone();
    sqlite.create_scalar_function("randomblob", 1, FunctionFlags::SQLITE_UTF8, move |ctx| {
        // As SQLite's: a length below 1 gives one byte.
        let n = match ctx.get_raw(0) {
            ValueRef::Integer(i) => i,
            ValueRef::Real(f) => f as i64,
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0),
            _ => 0,
        }
        .clamp(1, 1_000_000_000) as usize;
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            out.extend_from_slice(&draw(&s).to_le_bytes());
        }
        out.truncate(n);
        Ok(out)
    })
}

impl<T: Transport + 'static> Connection<T> {
    pub(crate) fn open(db: Database<T>) -> Result<Self> {
        let sqlite = db.sqlite.connect()?;
        let random = Arc::new(AtomicU64::new(0));
        install_random(&sqlite, &random)?;
        Ok(Connection {
            db,
            sqlite,
            recorded: RefCell::new(Vec::new()),
            clock: Cell::new(None),
            next_clock: Cell::new(None),
            random,
            origin: Cell::new(None),
            past: RefCell::new(Vec::new()),
        })
    }

    /// The database this connection belongs to.
    pub fn database(&self) -> &Database<T> {
        &self.db
    }

    /// Run one statement; returns the number of rows it changed.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.run(sql, params, true)?.changed)
    }

    /// Run one statement and return its rows. A query ending in `AS OF '<commit or time>'`
    /// reads the database as it was then.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        Ok(self.run(sql, params, true)?.rows)
    }

    /// The first column of the first row, or `Null`.
    pub fn query_value(&self, sql: &str, params: &[Value]) -> Result<Value> {
        Ok(self.query(sql, params)?.value())
    }

    /// Run one statement and report both its rows and the rows it changed.
    pub fn run_statement(&self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.run(sql, params, true)
    }

    /// Run several statements separated by semicolons, stopping at the first error.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        for statement in sql::split_statements(sql) {
            self.run(statement, &[], true)?;
        }
        Ok(())
    }

    /// Run `body` in a transaction and commit it. A refused commit rolls back and runs `body`
    /// again, with randomized back-off, until it commits or the attempts run out
    /// ([`Error::Serialization`]). An error from `body` rolls back and is returned. `body` may
    /// run more than once, so it should not have effects outside the database; every run sees
    /// the same time and random values.
    pub fn transaction<R>(&self, body: impl FnMut(&Transaction<'_, T>) -> Result<R>) -> Result<R> {
        self.transaction_with(Clock::fresh(), body)
    }

    /// Run `statements` as one transaction. A refused commit runs them all again on the new
    /// head (the logical rebase), so this suits callers that send statements rather than code,
    /// such as an HTTP API. A statement that fails ends the transaction with its error.
    pub fn run_transaction(&self, statements: &[Statement]) -> Result<Vec<Outcome>> {
        self.transaction_with(Clock::fresh(), |tx| run_all(tx, statements))
    }

    /// Run a commit's changelog again, as one transaction with the time and random seed it
    /// recorded, so `datetime('now')`, `random()` and `randomblob()` give what they gave then.
    /// On the commit's parent it reproduces the commit.
    pub fn replay(&self, changelog: &Changelog) -> Result<Vec<Outcome>> {
        let clock = Clock {
            now: changelog.now,
            seed: changelog.seed,
        };
        self.transaction_with(clock, |tx| run_all(tx, &changelog.statements))
    }

    /// Merge another database (another branch, or a fork) into this one: replay each of its
    /// commits since the two diverged, oldest first, one commit each, with the time and random
    /// seed it recorded. Commits whose changes this database already has are skipped, so
    /// merging again continues where the last merge stopped. A commit whose statements fail
    /// here stops the merge ([`Error::Merge`]); the commits before it stay merged.
    pub fn merge_from<U: Transport + 'static>(&self, source: &Database<U>) -> Result<MergeReport> {
        let runtime = self.db.sqlite.runtime().clone();
        let plan = runtime.block_on(crate::plan_merge(&self.db, source))?;
        let mut report = MergeReport {
            merged: Vec::new(),
            skipped: plan.skipped,
        };
        for (commit, changelog) in plan.replay {
            match self.replay_from(&changelog, commit) {
                Ok(()) => {
                    let head = self
                        .last_commit()
                        .map_or_else(|| self.db.head(), |c| c.head);
                    report.merged.push((commit, head));
                }
                Err((failed, reason)) => {
                    return Err(Error::Merge {
                        merged: report.merged.len(),
                        commit,
                        statement: failed
                            .and_then(|i| changelog.statements.get(i))
                            .map_or_else(|| "COMMIT".into(), |s| s.sql.clone()),
                        reason: reason.to_string(),
                    });
                }
            }
        }
        Ok(report)
    }

    /// Replay another branch's commit as one transaction that records it as its origin. On
    /// failure, which statement failed (`None`: the commit itself).
    fn replay_from(
        &self,
        changelog: &Changelog,
        origin: ObjectId,
    ) -> std::result::Result<(), (Option<usize>, Error)> {
        let clock = Clock {
            now: changelog.now,
            seed: changelog.seed,
        };
        let failed = Cell::new(None);
        self.origin.set(Some(origin));
        let result = self.transaction_with(clock, |tx| {
            failed.set(None);
            for (i, s) in changelog.statements.iter().enumerate() {
                tx.run_statement(&s.sql, &s.params)
                    .inspect_err(|_| failed.set(Some(i)))?;
            }
            Ok(())
        });
        self.origin.set(None);
        result.map_err(|e| (failed.get(), e))
    }

    /// `CREATE BRANCH`, `DROP BRANCH` and `MERGE BRANCH`, which act on refs, not on SQLite.
    fn branch(&self, command: BranchCommand) -> Result<Outcome> {
        if self.in_transaction() {
            return Err(Error::Invalid(
                "branch statements cannot run inside a transaction".into(),
            ));
        }
        let runtime = self.db.sqlite.runtime().clone();
        let one_row = |columns: &[&str], row: Vec<Value>| Outcome {
            rows: Rows {
                columns: columns.iter().map(|c| c.to_string()).collect(),
                rows: vec![row],
            },
            changed: 0,
        };
        match command {
            BranchCommand::Create { name, from } => {
                let head = runtime.block_on(self.db.create_branch(&name, from.as_deref()))?;
                Ok(one_row(
                    &["branch", "head"],
                    vec![name.into(), head.to_hex().into()],
                ))
            }
            BranchCommand::Drop { name } => {
                runtime.block_on(self.db.drop_branch(&name))?;
                Ok(Outcome::default())
            }
            BranchCommand::Merge { source, into } => {
                let source = runtime.block_on(self.db.open_branch(&source))?;
                let report = match into {
                    Some(target) if crate::branch_ref(&target)? != self.db.config.store.branch => {
                        let target = runtime.block_on(self.db.open_branch(&target))?;
                        target.connect()?.merge_from(&source)?
                    }
                    _ => self.merge_from(&source)?,
                };
                let head = report
                    .merged
                    .last()
                    .map_or_else(|| self.db.head(), |(_, head)| *head);
                Ok(one_row(
                    &["merged", "skipped", "head"],
                    vec![
                        Value::Integer(report.merged.len() as i64),
                        Value::Integer(report.skipped as i64),
                        head.to_hex().into(),
                    ],
                ))
            }
        }
    }

    fn transaction_with<R>(
        &self,
        clock: Clock,
        mut body: impl FnMut(&Transaction<'_, T>) -> Result<R>,
    ) -> Result<R> {
        if !self.sqlite.is_autocommit() {
            return Err(Error::Invalid(
                "a transaction is already open on this connection".into(),
            ));
        }
        let attempts = self.db.config.max_attempts.max(1);
        for attempt in 1..=attempts {
            self.next_clock.set(Some(clock));
            self.statement("BEGIN", &[])?;
            let tx = Transaction { conn: self };
            match body(&tx).and_then(|value| self.statement("COMMIT", &[]).map(|_| value)) {
                Ok(value) => return Ok(value),
                Err(Error::Conflict) => {
                    self.rollback_quietly();
                    if attempt < attempts {
                        backoff(attempt);
                    }
                }
                Err(e) => {
                    self.rollback_quietly();
                    return Err(e);
                }
            }
        }
        Err(Error::Serialization { attempts })
    }

    /// Whether a transaction is open (after `BEGIN`, before `COMMIT` or `ROLLBACK`).
    pub fn in_transaction(&self) -> bool {
        !self.sqlite.is_autocommit()
    }

    /// The rowid of the last row this connection inserted (0 if none), as SQLite's
    /// `last_insert_rowid()` has it.
    pub fn last_insert_rowid(&self) -> i64 {
        self.sqlite.last_insert_rowid()
    }

    /// What this connection's last commit did.
    pub fn last_commit(&self) -> Option<Committed> {
        octopage_sqlite::last_commit(&self.sqlite)
    }

    fn rollback_quietly(&self) {
        if !self.sqlite.is_autocommit() {
            let _ = self.sqlite.execute_batch("ROLLBACK");
        }
        self.recorded.borrow_mut().clear();
        self.clock.set(None);
    }

    /// A statement, from the API: `AS OF` queries go to the past; outside a transaction a
    /// refused commit is retried when `retry` is set, with the same clock.
    fn run(&self, sql: &str, params: &[Value], retry: bool) -> Result<Outcome> {
        if let Some((query, target)) = sql::split_as_of(sql) {
            return self.query_past(query, &target, params);
        }
        if let Some(command) = sql::branch_command(sql) {
            return self.branch(command);
        }
        if !retry || !self.sqlite.is_autocommit() {
            return self.statement(sql, params);
        }
        let clock = Clock::fresh();
        let attempts = self.db.config.max_attempts.max(1);
        for attempt in 1..=attempts {
            self.next_clock.set(Some(clock));
            match self.statement(sql, params) {
                Err(Error::Conflict) if attempt < attempts => backoff(attempt),
                Err(Error::Conflict) => break,
                other => return other,
            }
        }
        Err(Error::Serialization { attempts })
    }

    /// One statement on the live database, recorded for the changelog.
    fn statement(&self, sql: &str, params: &[Value]) -> Result<Outcome> {
        octopage_sqlite::take_last_error(); // a cause left over from before is not this one's
        let keyword = sql::leading_keyword(sql);
        let autocommit = self.sqlite.is_autocommit();
        if autocommit {
            // This statement starts a transaction (or is one).
            self.recorded.borrow_mut().clear();
            self.clock
                .set(Some(self.next_clock.take().unwrap_or_else(Clock::fresh)));
        }
        let clock = match self.clock.get() {
            Some(clock) => clock,
            None => {
                let clock = Clock::fresh();
                self.clock.set(Some(clock));
                clock
            }
        };
        let mut stmt = self.sqlite.prepare(sql)?;
        // Record what changes data, and the savepoint structure around it, so the changelog
        // can be run again to the same effect.
        let entry = (!stmt.readonly()
            || matches!(
                keyword,
                Keyword::Savepoint | Keyword::Release | Keyword::RollbackTo
            ))
        .then(|| Statement::new(sql.trim(), params));
        // Hand the VFS the changelog before any statement that may commit.
        if autocommit || matches!(keyword, Keyword::Commit | Keyword::Release) {
            let mut statements = self.recorded.borrow().clone();
            statements.extend(entry.clone());
            let changelog = (!statements.is_empty()).then(|| {
                changelog::encode(&Changelog {
                    now: clock.now,
                    seed: clock.seed,
                    statements,
                    origin: self.origin.get(),
                })
            });
            octopage_sqlite::set_changelog(&self.sqlite, changelog)?;
        }
        // Each statement draws random values from its own stream, keyed by its place among the
        // recorded statements: a statement that fails (and is not recorded) cannot shift the
        // values later ones draw, in the original run or in a replay.
        let position = self.recorded.borrow().len() as u64;
        self.random
            .store(mix(clock.seed ^ mix(position + 1)), Ordering::Relaxed);
        let before = self.sqlite.total_changes();
        let result = octopage_sqlite::with_clock(clock.now, || collect(&mut stmt, params));
        drop(stmt);
        let changed = self.sqlite.total_changes() - before;
        if self.sqlite.is_autocommit() {
            // The transaction ended.
            self.recorded.borrow_mut().clear();
            self.clock.set(None);
        } else if let (Some(entry), true) = (entry, result.is_ok()) {
            self.recorded.borrow_mut().push(entry);
        }
        let result = match result {
            Err(Error::SecretBlocked { .. }) => {
                let blocked = octopage_sqlite::last_blocked(&self.sqlite);
                Err(Error::SecretBlocked {
                    places: blocked
                        .as_ref()
                        .map(|b| b.places.clone())
                        .unwrap_or_default(),
                    message: blocked.map(|b| b.message).unwrap_or_default(),
                })
            }
            other => other,
        };
        Ok(Outcome {
            rows: result?,
            changed,
        })
    }

    /// A query on the database as of `target`.
    fn query_past(&self, sql: &str, target: &str, params: &[Value]) -> Result<Outcome> {
        let commit = self.db.sqlite.runtime().block_on(self.db.resolve(target))?;
        let mut past = self.past.borrow_mut();
        if !past.iter().any(|(c, _)| *c == commit) {
            let conn = self.db.at(commit).sqlite.connect()?;
            install_random(&conn, &self.random)?;
            if past.len() == PAST_CONNECTIONS {
                past.remove(0);
            }
            past.push((commit, conn));
        }
        let (_, conn) = past.iter().find(|(c, _)| *c == commit).unwrap();
        let mut stmt = conn.prepare(sql)?;
        let now = self
            .clock
            .get()
            .map_or_else(|| Clock::fresh().now, |c| c.now);
        Ok(Outcome {
            rows: octopage_sqlite::with_clock(now, || collect(&mut stmt, params))?,
            changed: 0,
        })
    }
}

fn collect(stmt: &mut rusqlite::Statement<'_>, params: &[Value]) -> Result<Rows> {
    let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let mut out = Vec::new();
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        let values = (0..columns.len())
            .map(|i| row.get_ref(i).map(Value::from))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push(values);
    }
    Ok(Rows { columns, rows: out })
}

fn run_all<T: Transport + 'static>(
    tx: &Transaction<'_, T>,
    statements: &[Statement],
) -> Result<Vec<Outcome>> {
    statements
        .iter()
        .map(|s| tx.run_statement(&s.sql, &s.params))
        .collect()
}

impl<T: Transport + 'static> Transaction<'_, T> {
    /// Run one statement in the transaction; returns the rows it changed.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.conn.run(sql, params, false)?.changed)
    }

    /// Run one statement in the transaction and return its rows.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        Ok(self.conn.run(sql, params, false)?.rows)
    }

    /// The first column of the first row, or `Null`.
    pub fn query_value(&self, sql: &str, params: &[Value]) -> Result<Value> {
        Ok(self.query(sql, params)?.value())
    }

    /// Run one statement and report both its rows and the rows it changed.
    pub fn run_statement(&self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.conn.run(sql, params, false)
    }

    /// Run several statements, separated by semicolons, in the transaction.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        for statement in sql::split_statements(sql) {
            self.conn.run(statement, &[], false)?;
        }
        Ok(())
    }
}
