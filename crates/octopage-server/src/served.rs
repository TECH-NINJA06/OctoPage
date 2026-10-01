use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use octopage::{Connection, Database, SmartHttp};
use tokio::sync::OwnedMutexGuard;

use crate::error::{ApiError, ApiResult};
use crate::governor::Governed;
use crate::meta::{DatabaseRecord, now};

/// The transport every served database uses: git over HTTPS, paced per installation.
pub type Remote = Governed<SmartHttp>;
/// A served database.
pub type Db = Database<Remote>;
/// A connection to one.
pub type Conn = Connection<Remote>;

/// Read connections kept per database.
const READERS: usize = 4;

/// An open database.
pub struct Served {
    pub record: Mutex<DatabaseRecord>,
    pub db: Db,
    writer: Arc<tokio::sync::Mutex<()>>,
    conn: Arc<Mutex<Option<Conn>>>,
    readers: Arc<Mutex<Vec<Conn>>>,
    pub last_used: AtomicI64,
    /// Where its pages are cached (one directory per repository).
    pub cache: PathBuf,
}

impl Served {
    pub fn new(record: DatabaseRecord, db: Db, cache: PathBuf) -> Arc<Self> {
        Arc::new(Served {
            record: Mutex::new(record),
            db,
            writer: Arc::new(tokio::sync::Mutex::new(())),
            conn: Arc::new(Mutex::new(None)),
            readers: Arc::new(Mutex::new(Vec::new())),
            last_used: AtomicI64::new(now()),
            cache,
        })
    }

    pub fn id(&self) -> String {
        self.record.lock().unwrap().id.clone()
    }

    pub fn touch(&self) {
        self.last_used.store(now(), Ordering::Relaxed);
    }

    /// Run `work` on the writer connection, in turn.
    pub async fn write<R: Send + 'static>(
        &self,
        work: impl FnOnce(&Conn) -> octopage::Result<R> + Send + 'static,
    ) -> ApiResult<R> {
        self.touch();
        let _turn = self.writer.clone().lock_owned().await;
        let (db, conn) = (self.db.clone(), self.conn.clone());
        tokio::task::spawn_blocking(move || {
            let mut slot = conn.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(db.connect()?);
            }
            let conn = slot.as_ref().expect("just opened");
            let result = work(conn);
            // The writer is shared: never leave it inside a transaction.
            if conn.in_transaction() {
                let _ = conn.execute("ROLLBACK", &[]);
            }
            result
        })
        .await
        .map_err(|e| ApiError::internal(format!("the statement's worker stopped: {e}")))?
        .map_err(ApiError::from)
    }

    /// Run `work` on a read connection, beside the writer.
    pub async fn read<R: Send + 'static>(
        &self,
        work: impl FnOnce(&Conn) -> octopage::Result<R> + Send + 'static,
    ) -> ApiResult<R> {
        self.touch();
        let (db, readers) = (self.db.clone(), self.readers.clone());
        tokio::task::spawn_blocking(move || {
            let pooled = readers.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let conn = match pooled {
                Some(conn) => conn,
                None => db.connect()?,
            };
            let result = work(&conn);
            let mut pool = readers.lock().unwrap_or_else(|e| e.into_inner());
            if pool.len() < READERS && !conn.in_transaction() {
                pool.push(conn);
            }
            result
        })
        .await
        .map_err(|e| ApiError::internal(format!("the query's worker stopped: {e}")))?
        .map_err(ApiError::from)
    }

    /// Wait for the writer's turn, and keep it (an interactive transaction).
    pub async fn take_turn(&self, wait: Duration) -> ApiResult<OwnedMutexGuard<()>> {
        tokio::time::timeout(wait, self.writer.clone().lock_owned())
            .await
            .map_err(|_| {
                ApiError::new(
                    axum::http::StatusCode::CONFLICT,
                    "busy",
                    "another transaction on this database is still open; try again",
                )
            })
    }
}

/// Whether a statement only reads: it may run beside the writer.
pub fn reads_only(sql: &str) -> bool {
    let word: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(word.as_str(), "SELECT" | "VALUES" | "EXPLAIN")
}

/// An interactive transaction.
pub struct Tx {
    pub id: String,
    pub database: String,
    pub user: i64,
    conn: Arc<Mutex<Option<Conn>>>,
    _turn: OwnedMutexGuard<()>,
    deadline: Mutex<Instant>,
    idle: Duration,
}

impl Tx {
    /// Open one on `served` for `user`: its own connection, holding the writer's turn.
    pub async fn begin(served: &Arc<Served>, user: i64, idle: Duration) -> ApiResult<Arc<Tx>> {
        let turn = served.take_turn(idle).await?;
        let db = served.db.clone();
        let conn = tokio::task::spawn_blocking(move || -> octopage::Result<Conn> {
            let conn = db.connect()?;
            conn.execute("BEGIN", &[])?;
            Ok(conn)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
        served.touch();
        Ok(Arc::new(Tx {
            id: format!("tx_{}", crate::meta::random_hex(8)),
            database: served.id(),
            user,
            conn: Arc::new(Mutex::new(Some(conn))),
            _turn: turn,
            deadline: Mutex::new(Instant::now() + idle),
            idle,
        }))
    }

    pub fn expired(&self) -> bool {
        Instant::now() > *self.deadline.lock().unwrap()
    }

    /// Run `work` in the transaction.
    pub async fn run<R: Send + 'static>(
        &self,
        work: impl FnOnce(&Conn) -> octopage::Result<R> + Send + 'static,
    ) -> ApiResult<R> {
        *self.deadline.lock().unwrap() = Instant::now() + self.idle;
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let slot = conn.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_ref() {
                Some(conn) => work(conn),
                None => Err(octopage::Error::Invalid("the transaction has ended".into())),
            }
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::from)
    }

    /// End it with `COMMIT` or `ROLLBACK`; the connection closes, and the writer's turn passes.
    pub async fn finish(&self, statement: &'static str) -> ApiResult<Option<octopage::ObjectId>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let taken = conn.lock().unwrap_or_else(|e| e.into_inner()).take();
            let Some(conn) = taken else {
                return Err(octopage::Error::Invalid("the transaction has ended".into()));
            };
            conn.execute(statement, &[])?;
            Ok(conn.last_commit().filter(|c| c.published).map(|c| c.head))
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::from)
    }
}

/// Open transactions, by id.
#[derive(Default)]
pub struct Transactions {
    open: Mutex<HashMap<String, Arc<Tx>>>,
}

impl Transactions {
    pub fn insert(&self, tx: Arc<Tx>) {
        self.open.lock().unwrap().insert(tx.id.clone(), tx);
    }

    pub fn get(&self, id: &str) -> Option<Arc<Tx>> {
        self.open.lock().unwrap().get(id).cloned()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Tx>> {
        self.open.lock().unwrap().remove(id)
    }

    pub fn len(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a transaction is open on `database`.
    pub fn on(&self, database: &str) -> bool {
        self.open
            .lock()
            .unwrap()
            .values()
            .any(|t| t.database == database)
    }

    /// Remove the transactions idle past their timeout; dropping one rolls it back.
    pub fn expire(&self) -> usize {
        let mut open = self.open.lock().unwrap();
        let before = open.len();
        open.retain(|_, tx| !tx.expired());
        before - open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_that_only_read() {
        assert!(reads_only("  select 1"));
        assert!(reads_only("SELECT * FROM t AS OF 'abc'"));
        assert!(reads_only("VALUES(1)"));
        assert!(!reads_only(
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"
        ));
        assert!(!reads_only("INSERT INTO t VALUES(1)"));
        assert!(!reads_only("PRAGMA user_version = 3"));
    }
}
