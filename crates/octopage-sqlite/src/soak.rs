use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use octopage_git::Transport;
use rusqlite::{Connection, ErrorCode, params};

use crate::{Database, SchemaObject, Stats, is_conflict};

pub const ACCOUNTS: i64 = 200;
pub const OPENING_BALANCE: i64 = 1_000;
const COUNTERS: i64 = 4;
/// Each writer keeps its newest events and deletes older ones now and then.
const KEEP_EVENTS: i64 = 30;
/// A writer gives up on one transaction after this many failed attempts.
const MAX_ATTEMPTS: u32 = 500;

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub writers: usize,
    pub transactions: usize,
    /// Commits refused after a lost race on the same pages, and run again.
    pub refused: u64,
    /// Lost races that did not overlap, rebased inside the VFS.
    pub rebases: u64,
    /// Commits whose outcome was unknown (or that failed with an I/O error), settled by reading
    /// the writer's own row.
    pub unknown: u64,
    /// Of those, commits that had in fact landed.
    pub late_landings: u64,
    /// Git commits published (transactions, schema changes and vacuums).
    pub published: u64,
    /// Pages SQLite rewrote unchanged, left out of commits.
    pub unchanged_pages: u64,
    pub schema_changes: u64,
    pub vacuums: u64,
    pub elapsed: Duration,
}

/// Create the soak's tables on a new database. The layout decides which races overlap: each
/// writer's row fills a page of its own and each writer appends to its own events table, while
/// accounts spread over about ten pages and the counters share one. So a transfer racing
/// another writer's transfer is often disjoint (rebased), and counter bumps always collide.
pub fn setup(conn: &Connection, writers: usize) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE accounts(id INTEGER PRIMARY KEY, balance INTEGER NOT NULL,
                               filler BLOB NOT NULL);
         CREATE TABLE writers(w INTEGER PRIMARY KEY, seq INTEGER NOT NULL,
                              bumps INTEGER NOT NULL, pruned INTEGER NOT NULL,
                              filler BLOB NOT NULL);
         CREATE TABLE counters(id INTEGER PRIMARY KEY, n INTEGER NOT NULL);",
    )?;
    for id in 1..=ACCOUNTS {
        conn.execute(
            "INSERT INTO accounts VALUES(?1, ?2, zeroblob(200))",
            params![id, OPENING_BALANCE],
        )?;
    }
    for w in 0..writers {
        conn.execute(
            "INSERT INTO writers VALUES(?1, 0, 0, 0, zeroblob(3000))",
            [w as i64],
        )?;
        conn.execute_batch(&format!(
            "CREATE TABLE {}(seq INTEGER PRIMARY KEY, body BLOB NOT NULL)",
            events(w)
        ))?;
    }
    for id in 1..=COUNTERS {
        conn.execute("INSERT INTO counters VALUES(?1, 0)", [id])?;
    }
    conn.execute_batch("COMMIT")
}

/// Writer `w`'s events table.
fn events(w: usize) -> String {
    format!("events_{w}")
}

fn mix(mut x: u64) -> u64 {
    // splitmix64
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The event body writer `w` inserts in transaction `seq`: recomputable, so the check can
/// compare every byte. One in ten is 3–20 KB, which takes overflow pages.
pub fn event_body(w: usize, seq: i64) -> Vec<u8> {
    let h = mix(((w as u64) << 32) | seq as u64);
    let len = if h.is_multiple_of(10) {
        3_000 + (h >> 8) % 17_000
    } else {
        20 + (h >> 8) % 200
    } as usize;
    format!("{w}:{seq};")
        .into_bytes()
        .into_iter()
        .cycle()
        .take(len)
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Transfer {
        from: i64,
        to: i64,
        amount: i64,
    },
    Bump {
        counter: i64,
    },
    Prune,
    /// Check the snapshot from inside the transaction: the total and this writer's events.
    Audit,
    /// Rewrite rows with their own values: SQLite writes the pages, the VFS leaves them out.
    Rewrite {
        residue: i64,
    },
    /// Writer 0 only: create or drop an index.
    Schema,
    /// Writer 0 only: after the transaction, `VACUUM` (which rewrites and truncates the file).
    Vacuum,
}

fn draw(rng: &mut fastrand::Rng, w: usize) -> Action {
    match rng.u32(0..100) {
        0..40 => {
            let from = rng.i64(1..=ACCOUNTS);
            let mut to = rng.i64(1..=ACCOUNTS);
            if to == from {
                to = from % ACCOUNTS + 1;
            }
            Action::Transfer {
                from,
                to,
                amount: rng.i64(1..=100),
            }
        }
        40..60 => Action::Bump {
            counter: rng.i64(1..=COUNTERS),
        },
        60..75 => Action::Prune,
        75..90 => Action::Audit,
        90..96 => Action::Rewrite {
            residue: rng.i64(0..10),
        },
        96..99 if w == 0 => Action::Schema,
        99 if w == 0 => Action::Vacuum,
        _ => Action::Audit,
    }
}

enum Failure {
    Sql(rusqlite::Error),
    /// An invariant did not hold: a real bug, never retried.
    Broken(String),
}

impl From<rusqlite::Error> for Failure {
    fn from(e: rusqlite::Error) -> Self {
        Failure::Sql(e)
    }
}

/// One transaction of writer `w`: its step `seq`, one event, and `action`.
fn apply(conn: &Connection, w: usize, seq: i64, action: Action) -> Result<(), Failure> {
    let w64 = w as i64;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let (prev, pruned): (i64, i64) =
        conn.query_row("SELECT seq, pruned FROM writers WHERE w = ?1", [w64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if prev != seq - 1 {
        return Err(Failure::Broken(format!(
            "writer {w} is at step {prev} in the database but about to run step {seq}"
        )));
    }
    conn.execute(
        "UPDATE writers SET seq = ?1 WHERE w = ?2",
        params![seq, w64],
    )?;
    conn.execute(
        &format!("INSERT INTO {}(seq, body) VALUES(?1, ?2)", events(w)),
        params![seq, event_body(w, seq)],
    )?;
    match action {
        Action::Transfer { from, to, amount } => {
            conn.execute(
                "UPDATE accounts SET balance = balance - ?1 WHERE id = ?2",
                params![amount, from],
            )?;
            conn.execute(
                "UPDATE accounts SET balance = balance + ?1 WHERE id = ?2",
                params![amount, to],
            )?;
        }
        Action::Bump { counter } => {
            conn.execute("UPDATE counters SET n = n + 1 WHERE id = ?1", [counter])?;
            conn.execute("UPDATE writers SET bumps = bumps + 1 WHERE w = ?1", [w64])?;
        }
        Action::Prune => {
            let below = seq - KEEP_EVENTS;
            if below > pruned {
                conn.execute(
                    &format!("DELETE FROM {} WHERE seq <= ?1", events(w)),
                    [below],
                )?;
                conn.execute(
                    "UPDATE writers SET pruned = ?1 WHERE w = ?2",
                    params![below, w64],
                )?;
            }
        }
        Action::Audit => {
            let total: i64 =
                conn.query_row("SELECT sum(balance) FROM accounts", [], |r| r.get(0))?;
            if total != ACCOUNTS * OPENING_BALANCE {
                return Err(Failure::Broken(format!(
                    "a snapshot holds {total} in total, expected {}",
                    ACCOUNTS * OPENING_BALANCE
                )));
            }
            let events: i64 =
                conn.query_row(&format!("SELECT count(*) FROM {}", events(w)), [], |r| {
                    r.get(0)
                })?;
            if events != seq - pruned {
                return Err(Failure::Broken(format!(
                    "writer {w} sees {events} of its events at step {seq}, expected {}",
                    seq - pruned
                )));
            }
        }
        Action::Rewrite { residue } => {
            conn.execute(
                "UPDATE accounts SET balance = balance WHERE id % 10 = ?1",
                [residue],
            )?;
        }
        Action::Schema => {
            let exists: bool = conn.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'events_len'",
                [],
                |r| r.get(0),
            )?;
            conn.execute_batch(&if exists {
                "DROP INDEX events_len".to_string()
            } else {
                format!("CREATE INDEX events_len ON {}(length(body))", events(w))
            })?;
        }
        Action::Vacuum => {}
    }
    conn.execute_batch("COMMIT")?;
    Ok(())
}

/// Whether writer `w`'s step `seq` is in the database (after a commit of unknown outcome).
fn landed(conn: &Connection, w: usize, seq: i64) -> Result<bool, String> {
    let at: i64 = conn
        .query_row("SELECT seq FROM writers WHERE w = ?1", [w as i64], |r| {
            r.get(0)
        })
        .map_err(|e| format!("writer {w}: reading its step: {e}"))?;
    match at {
        at if at == seq => Ok(true),
        at if at == seq - 1 => Ok(false),
        at => Err(format!(
            "writer {w} is at step {at} while settling step {seq}"
        )),
    }
}

fn is_io(error: &rusqlite::Error) -> bool {
    error.sqlite_error_code() == Some(ErrorCode::SystemIoFailure)
}

fn writer<T: Transport + 'static>(
    db: &Arc<Database<T>>,
    w: usize,
    per_writer: usize,
    seed: u64,
    done: &AtomicUsize,
    progress: Option<usize>,
) -> Result<Report, String> {
    let conn = db.connect().map_err(|e| format!("writer {w}: {e}"))?;
    let mut rng = fastrand::Rng::with_seed(mix(seed ^ w as u64));
    let mut report = Report::default();
    for seq in 1..=per_writer as i64 {
        let action = draw(&mut rng, w);
        let mut attempts = 0;
        loop {
            attempts += 1;
            match apply(&conn, w, seq, action) {
                Ok(()) => break,
                Err(Failure::Broken(message)) => return Err(message),
                Err(Failure::Sql(e)) if is_conflict(&e) => report.refused += 1,
                Err(Failure::Sql(e)) if is_io(&e) => {
                    // Never run a step twice without checking whether it landed.
                    let _ = conn.execute_batch("ROLLBACK");
                    report.unknown += 1;
                    if landed(&conn, w, seq)? {
                        report.late_landings += 1;
                        break;
                    }
                }
                Err(Failure::Sql(e)) => return Err(format!("writer {w}, step {seq}: {e}")),
            }
            let _ = conn.execute_batch("ROLLBACK");
            if attempts >= MAX_ATTEMPTS {
                return Err(format!("writer {w}: step {seq} failed {attempts} times"));
            }
            // Randomized back-off, so one writer's streak of wins cannot starve the others.
            std::thread::sleep(Duration::from_millis(
                rng.u64(0..=2 * u64::from(attempts.min(25))),
            ));
        }
        if let Action::Schema = action {
            report.schema_changes += 1;
        }
        if let Action::Vacuum = action {
            // VACUUM cannot run inside a transaction. It rewrites every page, so it conflicts
            // with everything; a few tries, then move on.
            for _ in 0..5 {
                match conn.execute_batch("VACUUM") {
                    Ok(()) => {
                        report.vacuums += 1;
                        break;
                    }
                    Err(e) if is_conflict(&e) || is_io(&e) => {
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                    Err(e) => return Err(format!("writer {w}: VACUUM: {e}")),
                }
            }
        }
        report.transactions += 1;
        let n = done.fetch_add(1, Ordering::SeqCst) + 1;
        if progress.is_some_and(|every| n.is_multiple_of(every)) {
            eprintln!("  {n} transactions committed");
        }
    }
    Ok(report)
}

/// Run `per_writer` transactions on each database (one per writer, each its own client).
pub fn run<T: Transport + 'static>(
    writers: &[Arc<Database<T>>],
    per_writer: usize,
    seed: u64,
    progress: Option<usize>,
) -> Result<Report, String> {
    let started = Instant::now();
    let done = AtomicUsize::new(0);
    let results: Vec<Result<Report, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = writers
            .iter()
            .enumerate()
            .map(|(w, db)| {
                let done = &done;
                scope.spawn(move || writer(db, w, per_writer, seed, done, progress))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("a writer panicked".into())))
            .collect()
    });
    let mut total = Report {
        writers: writers.len(),
        ..Report::default()
    };
    for r in results {
        let r = r?;
        total.transactions += r.transactions;
        total.refused += r.refused;
        total.unknown += r.unknown;
        total.late_landings += r.late_landings;
        total.schema_changes += r.schema_changes;
        total.vacuums += r.vacuums;
    }
    for db in writers {
        total.rebases += Stats::get(&db.stats.rebases);
        total.published += Stats::get(&db.stats.published);
        total.unchanged_pages += Stats::get(&db.stats.unchanged_pages);
    }
    total.elapsed = started.elapsed();
    Ok(total)
}

/// The invariants that hold in every committed state: a clean integrity check and the total.
pub fn check_snapshot(conn: &Connection) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(e)?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}"));
    }
    let (accounts, total): (i64, i64) = conn
        .query_row("SELECT count(*), sum(balance) FROM accounts", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(e)?;
    if (accounts, total) != (ACCOUNTS, ACCOUNTS * OPENING_BALANCE) {
        return Err(format!(
            "{accounts} accounts holding {total}, expected {ACCOUNTS} holding {}",
            ACCOUNTS * OPENING_BALANCE
        ));
    }
    Ok(())
}

/// Check the database after `run`.
pub fn verify(conn: &Connection, writers: usize, per_writer: usize) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    check_snapshot(conn)?;
    let mut bumps = 0i64;
    for w in 0..writers {
        let (seq, b, pruned): (i64, i64, i64) = conn
            .query_row(
                "SELECT seq, bumps, pruned FROM writers WHERE w = ?1",
                [w as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(e)?;
        if seq != per_writer as i64 {
            return Err(format!(
                "writer {w} is at step {seq}, expected {per_writer}"
            ));
        }
        bumps += b;
        let mut query = conn
            .prepare(&format!("SELECT seq, body FROM {} ORDER BY seq", events(w)))
            .map_err(e)?;
        let events: Vec<(i64, Vec<u8>)> = query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e)?
            .collect::<rusqlite::Result<_>>()
            .map_err(e)?;
        let expected: Vec<i64> = (pruned + 1..=seq).collect();
        let got: Vec<i64> = events.iter().map(|(s, _)| *s).collect();
        if got != expected {
            return Err(format!(
                "writer {w}: events {}..={} expected, found {} events ({:?}..)",
                pruned + 1,
                seq,
                got.len(),
                got.first()
            ));
        }
        if let Some((s, _)) = events.iter().find(|(s, body)| *body != event_body(w, *s)) {
            return Err(format!("writer {w}: event {s} has the wrong bytes"));
        }
    }
    let counted: i64 = conn
        .query_row("SELECT sum(n) FROM counters", [], |r| r.get(0))
        .map_err(e)?;
    if counted != bumps {
        return Err(format!(
            "the counters add up to {counted}, but writers bumped them {bumps} times"
        ));
    }
    Ok(())
}

async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("check task failed: {e}"))?
}

/// The whole check after `run`, from a client of its own (call it on the async runtime): the
/// database (`verify`), the catalog blob against `sqlite_schema`, one commit per published
/// transaction since `commits_before` (the history length after `setup`), and a consistent
/// snapshot at a sample of the commits made since.
pub async fn verify_all<T: Transport + 'static>(
    checker: Arc<Database<T>>,
    writers: usize,
    per_writer: usize,
    commits_before: usize,
    report: &Report,
) -> Result<(), String> {
    let c = checker.clone();
    let schema = blocking(move || {
        let conn = c.connect().map_err(|e| e.to_string())?;
        verify(&conn, writers, per_writer)?;
        SchemaObject::all(&conn).map_err(|e| e.to_string())
    })
    .await?;
    let catalog = checker
        .catalog()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no catalog blob")?;
    if catalog.objects != schema {
        return Err("the catalog blob does not match sqlite_schema".into());
    }
    let log = checker
        .store()
        .log(usize::MAX)
        .await
        .map_err(|e| e.to_string())?;
    let expected = commits_before as u64 + report.published + report.late_landings;
    if log.len() as u64 != expected {
        return Err(format!(
            "{} commits in history, expected {expected}: one per published transaction",
            log.len()
        ));
    }
    let since_setup = log.len() - commits_before + 1;
    for entry in log[..since_setup].iter().step_by(since_setup / 8 + 1) {
        let old = Database::at(
            checker.store().clone(),
            checker.runtime.clone(),
            entry.commit,
        );
        blocking(move || check_snapshot(&old.connect().map_err(|e| e.to_string())?))
            .await
            .map_err(|e| format!("commit {}: {e}", entry.commit))?;
    }
    Ok(())
}
