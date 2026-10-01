use std::sync::{Arc, Mutex, OnceLock};

use napi::bindgen_prelude::*;
use napi::{Env, Task};
use napi_derive::napi;
use octopage::{
    Config, Database as Db, Error as OctoError, InMemory, SmartHttp, Statement, Unlock, Value,
};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("octopage")
            .build()
            .expect("a Tokio runtime")
    })
}

/// The code for an OctoPage error: the same words the HTTP API uses.
fn code(error: &OctoError) -> &'static str {
    use octopage_pagestore::Error as Store;
    match error {
        OctoError::Sql(e) => match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::ConstraintViolation) => "constraint",
            _ => "sql",
        },
        OctoError::Conflict => "conflict",
        OctoError::Serialization { .. } => "busy",
        OctoError::Unavailable(_) => "unavailable",
        OctoError::OutcomeUnknown => "outcome_unknown",
        OctoError::Full(_) => "full",
        OctoError::SecretBlocked { .. } => "secret_blocked",
        OctoError::Store(Store::Locked | Store::WrongKey) => "locked",
        OctoError::Store(Store::NoDatabase(_)) => "not_found",
        OctoError::Invalid(_) | OctoError::AsOf { .. } => "invalid",
        _ => "error",
    }
}

fn js_err(error: OctoError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("[{}] {error}", code(&error)),
    )
}

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::new(Status::InvalidArg, format!("[invalid] {message}"))
}

#[doc(hidden)]
pub enum Inner {
    Remote(Db<SmartHttp>),
    Memory(Db<InMemory>),
}

#[doc(hidden)]
pub enum Conn {
    Remote(octopage::Connection<SmartHttp>),
    Memory(octopage::Connection<InMemory>),
}

macro_rules! each {
    ($value:expr, $inner:ident => $body:expr) => {
        match $value {
            Conn::Remote($inner) => $body,
            Conn::Memory($inner) => $body,
        }
    };
}

/// A parameter: `null`, a boolean, a number, a bigint, a string or a Buffer.
pub type Param = Either6<Null, bool, f64, BigInt, String, Buffer>;

/// A value in a result.
pub type Cell = Either5<Null, f64, BigInt, String, Buffer>;

fn to_value(param: Param) -> Result<Value> {
    Ok(match param {
        Either6::A(_) => Value::Null,
        Either6::B(b) => Value::Integer(b as i64),
        Either6::C(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0 => {
            Value::Integer(n as i64)
        }
        Either6::C(n) => Value::Real(n),
        Either6::D(big) => {
            let (value, lossless) = big.get_i64();
            if !lossless {
                return Err(invalid("a bigint beyond 64 bits"));
            }
            Value::Integer(value)
        }
        Either6::E(s) => Value::Text(s),
        Either6::F(buffer) => Value::Blob(buffer.to_vec()),
    })
}

fn to_cell(value: Value, bigints: bool) -> Cell {
    const EXACT: i64 = 1 << 53;
    match value {
        Value::Null => Either5::A(Null),
        Value::Integer(i) if !bigints && (-EXACT..=EXACT).contains(&i) => Either5::B(i as f64),
        Value::Integer(i) => Either5::C(BigInt::from(i)),
        Value::Real(f) => Either5::B(f),
        Value::Text(t) => Either5::D(t),
        Value::Blob(b) => Either5::E(Buffer::from(b)),
    }
}

/// How to open a database.
#[napi(object)]
#[derive(Default)]
pub struct OpenOptions {
    /// A token to sign in with (a fine-grained token, or an app installation token).
    pub token: Option<String>,
    /// The branch holding the database (default `main`).
    pub branch: Option<String>,
    pub passphrase: Option<String>,
    pub recovery_key: Option<String>,
    /// Create the database if the branch has none.
    pub create: Option<bool>,
    /// When creating, encrypt it (with `passphrase`).
    pub encrypt: Option<bool>,
    /// Keep fetched pages here between runs.
    pub cache_dir: Option<String>,
    /// The user to sign in as at a URL (default `x-access-token`).
    pub user: Option<String>,
    /// A new database's page size: 4096, 8192 or 16384.
    pub page_size: Option<u32>,
}

/// What one statement did.
#[napi(object, object_from_js = false)]
pub struct Outcome {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Rows inserted, updated or deleted.
    pub changed: i64,
    /// The rowid of the last row this connection inserted.
    pub last_insert_rowid: i64,
    /// The commit this statement made, if it made one.
    pub commit: Option<String>,
}

/// A statement for `batch`.
#[napi(object, object_to_js = false)]
pub struct StatementInput {
    pub sql: String,
    pub params: Option<Vec<Param>>,
}

#[doc(hidden)]
pub struct Ran {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    changed: u64,
    rowid: i64,
    commit: Option<String>,
}

fn outcome(ran: Ran, bigints: bool) -> Outcome {
    Outcome {
        columns: ran.columns,
        rows: ran
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(|v| to_cell(v, bigints)).collect())
            .collect(),
        changed: ran.changed as i64,
        last_insert_rowid: ran.rowid,
        commit: ran.commit,
    }
}

// ------------------------------------------------------------------------------ opening

pub struct OpenTask {
    location: String,
    options: OpenOptions,
}

impl Task for OpenTask {
    type Output = (Inner, Option<String>);
    type JsValue = Database;

    fn compute(&mut self) -> Result<Self::Output> {
        let o = &self.options;
        let mut config = Config::default();
        let branch = o.branch.clone().unwrap_or_else(|| "main".into());
        config.store.branch = if branch.starts_with("refs/") {
            branch
        } else {
            format!("refs/heads/{branch}")
        };
        config.store.cache_dir = o.cache_dir.clone().map(Into::into);
        config.store.unlock = match (&o.recovery_key, &o.passphrase) {
            (Some(key), _) => Some(Unlock::RecoveryKey(key.clone())),
            (None, Some(p)) => Some(Unlock::Passphrase(p.clone())),
            (None, None) => None,
        };
        if let Some(size) = o.page_size {
            config.page_size = size as usize;
        }
        let encryption = match (o.encrypt.unwrap_or(false), &o.passphrase) {
            (true, Some(p)) => Some(p.clone()),
            (true, None) => return Err(invalid("encrypt needs a passphrase")),
            (false, _) => None,
        };
        let create = o.create.unwrap_or(false);
        let (location, token) = (self.location.clone(), o.token.clone());
        let user = o.user.clone().unwrap_or_else(|| "x-access-token".into());
        runtime()
            .block_on(async move {
                async fn open<T: octopage::Transport + 'static>(
                    make: impl Fn() -> octopage::Result<T>,
                    config: Config,
                    create: bool,
                    encryption: Option<String>,
                ) -> octopage::Result<(Db<T>, Option<String>)> {
                    match Db::open(make()?, config.clone()).await {
                        Err(OctoError::Store(octopage_pagestore::Error::NoDatabase(_)))
                            if create =>
                        {
                            match encryption {
                                None => Ok((Db::create(make()?, config).await?, None)),
                                Some(p) => {
                                    let (db, key) = Db::create_encrypted(
                                        make()?,
                                        config,
                                        octopage::Encryption::passphrase(p),
                                    )
                                    .await?;
                                    Ok((db, Some(key.to_string())))
                                }
                            }
                        }
                        other => other.map(|db| (db, None)),
                    }
                }
                if location == ":memory:" {
                    let (db, key) = open(|| Ok(InMemory::new()), config, true, encryption).await?;
                    return Ok((Inner::Memory(db), key));
                }
                let make = || {
                    if location.contains("://") {
                        octopage::remote(&location, token.as_deref(), &user)
                    } else {
                        octopage::github(&location, token.as_deref())
                    }
                };
                let (db, key) = open(make, config, create, encryption).await?;
                Ok((Inner::Remote(db), key))
            })
            .map_err(js_err)
    }

    fn resolve(&mut self, _env: Env, (db, recovery): Self::Output) -> Result<Database> {
        Ok(Database {
            inner: Arc::new(db),
            recovery: Mutex::new(recovery),
        })
    }
}

/// A database kept in a git repository.
#[napi]
pub struct Database {
    inner: Arc<Inner>,
    recovery: Mutex<Option<String>>,
}

pub struct ConnectTask {
    inner: Arc<Inner>,
    bigints: bool,
}

impl Task for ConnectTask {
    type Output = Conn;
    type JsValue = Connection;

    fn compute(&mut self) -> Result<Conn> {
        match &*self.inner {
            Inner::Remote(d) => d.connect().map(Conn::Remote),
            Inner::Memory(d) => d.connect().map(Conn::Memory),
        }
        .map_err(js_err)
    }

    fn resolve(&mut self, _env: Env, conn: Conn) -> Result<Connection> {
        Ok(Connection {
            conn: Arc::new(Mutex::new(Some(conn))),
            bigints: self.bigints,
        })
    }
}

pub struct HeadTask {
    inner: Arc<Inner>,
}

impl Task for HeadTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<String> {
        let inner = self.inner.clone();
        runtime()
            .block_on(async move {
                match &*inner {
                    Inner::Remote(d) => d.refresh().await,
                    Inner::Memory(d) => d.refresh().await,
                }
            })
            .map(|h| h.to_hex())
            .map_err(js_err)
    }

    fn resolve(&mut self, _env: Env, head: String) -> Result<String> {
        Ok(head)
    }
}

#[napi]
impl Database {
    /// Open the database at `location`: "OWNER/NAME" on github.com, the URL of any smart-HTTP
    /// git remote, or ":memory:" for a scratch database in this process.
    #[napi(ts_return_type = "Promise<Database>")]
    pub fn open(location: String, options: Option<OpenOptions>) -> AsyncTask<OpenTask> {
        AsyncTask::new(OpenTask {
            location,
            options: options.unwrap_or_default(),
        })
    }

    /// A new connection. With `bigints`, every integer comes out as a bigint.
    #[napi(ts_return_type = "Promise<Connection>")]
    pub fn connect(&self, bigints: Option<bool>) -> AsyncTask<ConnectTask> {
        AsyncTask::new(ConnectTask {
            inner: self.inner.clone(),
            bigints: bigints.unwrap_or(false),
        })
    }

    /// Read the head now: its commit, as 40 hex digits.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn head(&self) -> AsyncTask<HeadTask> {
        AsyncTask::new(HeadTask {
            inner: self.inner.clone(),
        })
    }

    /// Where the database lives now: OWNER/NAME on GitHub.
    #[napi(getter)]
    pub fn location(&self) -> Option<String> {
        match &*self.inner {
            Inner::Remote(d) => d.location(),
            Inner::Memory(d) => d.location(),
        }
    }

    /// The recovery key of a database just created encrypted: once, then null.
    #[napi(getter)]
    pub fn recovery_key(&self) -> Option<String> {
        self.recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

// ------------------------------------------------------------------------------ connections

type Shared = Arc<Mutex<Option<Conn>>>;

fn with_conn<R>(shared: &Shared, work: impl FnOnce(&Conn) -> octopage::Result<R>) -> Result<R> {
    let slot = shared.lock().unwrap_or_else(|e| e.into_inner());
    let conn = slot
        .as_ref()
        .ok_or_else(|| Error::new(Status::GenericFailure, "[closed] the connection is closed"))?;
    work(conn).map_err(js_err)
}

pub struct RunTask {
    conn: Shared,
    sql: String,
    params: Vec<Value>,
    bigints: bool,
}

impl Task for RunTask {
    type Output = Ran;
    type JsValue = Outcome;

    fn compute(&mut self) -> Result<Ran> {
        let (sql, params) = (&self.sql, &self.params);
        with_conn(&self.conn, |conn| {
            each!(conn, c => {
                let before = c.last_commit();
                let outcome = c.run_statement(sql, params)?;
                let after = c.last_commit();
                let commit = after
                    .filter(|a| Some(*a) != before && a.published)
                    .map(|a| a.head.to_hex());
                Ok(Ran {
                    columns: outcome.rows.columns,
                    rows: outcome.rows.rows,
                    changed: outcome.changed,
                    rowid: c.last_insert_rowid(),
                    commit,
                })
            })
        })
    }

    fn resolve(&mut self, _env: Env, ran: Ran) -> Result<Outcome> {
        Ok(outcome(ran, self.bigints))
    }
}

pub struct BatchTask {
    conn: Shared,
    statements: Vec<Statement>,
}

impl Task for BatchTask {
    type Output = (Vec<i64>, Option<String>);
    type JsValue = BatchOutcome;

    fn compute(&mut self) -> Result<Self::Output> {
        let statements = &self.statements;
        with_conn(&self.conn, |conn| {
            each!(conn, c => {
                let before = c.last_commit();
                let outcomes = c.run_transaction(statements)?;
                let after = c.last_commit();
                let commit = after
                    .filter(|a| Some(*a) != before && a.published)
                    .map(|a| a.head.to_hex());
                Ok((outcomes.iter().map(|o| o.changed as i64).collect(), commit))
            })
        })
    }

    fn resolve(&mut self, _env: Env, (changed, commit): Self::Output) -> Result<BatchOutcome> {
        Ok(BatchOutcome { changed, commit })
    }
}

pub struct ScriptTask {
    conn: Shared,
    sql: String,
}

impl Task for ScriptTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<()> {
        let sql = &self.sql;
        with_conn(&self.conn, |conn| each!(conn, c => c.execute_batch(sql)))
    }

    fn resolve(&mut self, _env: Env, _: ()) -> Result<()> {
        Ok(())
    }
}

/// What `batch` did.
#[napi(object, object_from_js = false)]
pub struct BatchOutcome {
    /// Rows each statement changed.
    pub changed: Vec<i64>,
    /// The commit it made, if any.
    pub commit: Option<String>,
}

/// A connection: statements, queries and transactions.
#[napi]
pub struct Connection {
    conn: Shared,
    bigints: bool,
}

#[napi]
impl Connection {
    /// Run one statement (`BEGIN`, `COMMIT` and `ROLLBACK` included). Outside a transaction it
    /// commits by itself, running again after a refused commit.
    #[napi(ts_return_type = "Promise<Outcome>")]
    pub fn execute(&self, sql: String, params: Option<Vec<Param>>) -> Result<AsyncTask<RunTask>> {
        let params = params
            .unwrap_or_default()
            .into_iter()
            .map(to_value)
            .collect::<Result<Vec<_>>>()?;
        Ok(AsyncTask::new(RunTask {
            conn: self.conn.clone(),
            sql,
            params,
            bigints: self.bigints,
        }))
    }

    /// Run several statements separated by semicolons, without parameters, each as it would
    /// run on its own.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn execute_batch(&self, sql: String) -> AsyncTask<ScriptTask> {
        AsyncTask::new(ScriptTask {
            conn: self.conn.clone(),
            sql,
        })
    }

    /// Run statements as one transaction, running them all again after a refused commit.
    #[napi(ts_return_type = "Promise<BatchOutcome>")]
    pub fn batch(&self, statements: Vec<StatementInput>) -> Result<AsyncTask<BatchTask>> {
        let statements = statements
            .into_iter()
            .map(|s| {
                let params = s
                    .params
                    .unwrap_or_default()
                    .into_iter()
                    .map(to_value)
                    .collect::<Result<Vec<_>>>()?;
                Ok(Statement::new(s.sql, &params))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(AsyncTask::new(BatchTask {
            conn: self.conn.clone(),
            statements,
        }))
    }

    /// Whether a transaction is open.
    #[napi(getter)]
    pub fn in_transaction(&self) -> bool {
        let slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(conn) => each!(conn, c => c.in_transaction()),
            None => false,
        }
    }

    /// Close the connection, dropping an open transaction.
    #[napi]
    pub fn close(&self) {
        self.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// OctoPage's version.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
