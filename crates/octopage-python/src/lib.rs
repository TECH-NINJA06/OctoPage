use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use octopage::{
    Config, Database, Error as OctoError, InMemory, SmartHttp, Statement, Unlock, Value,
};
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyStopIteration};
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple,
};

create_exception!(
    _octopage,
    Warning,
    PyException,
    "PEP 249: important warnings."
);
create_exception!(
    _octopage,
    Error,
    PyException,
    "PEP 249: the base of OctoPage's errors."
);
create_exception!(_octopage, InterfaceError, Error, "Misuse of the interface.");
create_exception!(_octopage, DatabaseError, Error, "Errors of the database.");
create_exception!(
    _octopage,
    DataError,
    DatabaseError,
    "Bad data: a value out of range."
);
create_exception!(
    _octopage,
    OperationalError,
    DatabaseError,
    "Errors in the database's operation, not the program's."
);
create_exception!(
    _octopage,
    IntegrityError,
    DatabaseError,
    "A constraint failed: a unique key, a foreign key, NOT NULL."
);
create_exception!(
    _octopage,
    InternalError,
    DatabaseError,
    "OctoPage failed internally."
);
create_exception!(
    _octopage,
    ProgrammingError,
    DatabaseError,
    "A mistake in the program: bad SQL, a wrong number of parameters."
);
create_exception!(
    _octopage,
    NotSupportedError,
    DatabaseError,
    "Not supported."
);
create_exception!(
    _octopage,
    ConflictError,
    OperationalError,
    "The commit was refused: another client changed the same data first. Nothing was applied; \
     run the transaction again."
);
create_exception!(
    _octopage,
    UnavailableError,
    OperationalError,
    "The repository could not be reached, or throttled the request. Nothing was committed."
);
create_exception!(
    _octopage,
    OutcomeUnknownError,
    OperationalError,
    "The network failed while committing: whether the commit landed is unknown."
);
create_exception!(
    _octopage,
    FullError,
    OperationalError,
    "The write would grow the database past its live-size limit."
);
create_exception!(
    _octopage,
    LockedError,
    OperationalError,
    "The database is encrypted: open it with its passphrase or recovery key."
);

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

/// An OctoPage error as the Python exception for its kind.
fn py_err(error: OctoError) -> PyErr {
    use octopage_pagestore::Error as Store;
    let message = error.to_string();
    match &error {
        OctoError::Sql(e) => match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::ConstraintViolation) => IntegrityError::new_err(message),
            Some(rusqlite::ErrorCode::TooBig) => DataError::new_err(message),
            Some(rusqlite::ErrorCode::ApiMisuse) => InterfaceError::new_err(message),
            _ if message.contains("syntax error") => ProgrammingError::new_err(message),
            _ => OperationalError::new_err(message),
        },
        OctoError::Conflict | OctoError::Serialization { .. } => ConflictError::new_err(message),
        OctoError::Unavailable(_) => UnavailableError::new_err(message),
        OctoError::OutcomeUnknown => OutcomeUnknownError::new_err(message),
        OctoError::Full(_) => FullError::new_err(message),
        OctoError::Store(Store::Locked | Store::WrongKey) => LockedError::new_err(message),
        OctoError::Invalid(_) | OctoError::AsOf { .. } => ProgrammingError::new_err(message),
        _ => OperationalError::new_err(message),
    }
}

enum Db {
    Remote(Database<SmartHttp>),
    Memory(Database<InMemory>),
}

enum Conn {
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

/// A statement that only reads, which never opens a transaction by itself.
fn reads_only(sql: &str) -> bool {
    let word: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "SELECT"
            | "VALUES"
            | "EXPLAIN"
            | "BEGIN"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
    )
}

/// A Python value as a SQL value.
fn to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Integer(obj.extract::<bool>()? as i64));
    }
    if obj.is_instance_of::<PyInt>() {
        return obj
            .extract::<i64>()
            .map(Value::Integer)
            .map_err(|_| DataError::new_err("an integer beyond 64 bits"));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::Real(obj.extract::<f64>()?));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::Text(obj.extract::<String>()?));
    }
    if let Ok(bytes) = obj.cast::<PyBytes>() {
        return Ok(Value::Blob(bytes.as_bytes().to_vec()));
    }
    if let Ok(array) = obj.cast::<PyByteArray>() {
        return Ok(Value::Blob(array.to_vec()));
    }
    if obj.get_type().name()? == "memoryview" {
        let bytes = obj
            .py()
            .import("builtins")?
            .getattr("bytes")?
            .call1((obj,))?;
        return Ok(Value::Blob(bytes.cast::<PyBytes>()?.as_bytes().to_vec()));
    }
    Err(ProgrammingError::new_err(format!(
        "a parameter of type {} is not a SQL value (None, int, float, str, bytes)",
        obj.get_type().name()?
    )))
}

/// A SQL value as a Python value.
fn to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Null => py.None().into_bound(py),
        Value::Integer(i) => i.into_pyobject(py)?.into_any(),
        Value::Real(f) => f.into_pyobject(py)?.into_any(),
        Value::Text(t) => t.into_pyobject(py)?.into_any(),
        Value::Blob(b) => PyBytes::new(py, b).into_any(),
    })
}

/// Parameters: a sequence, for `?` or `?N` placeholders.
fn params(params: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    let Some(params) = params else {
        return Ok(Vec::new());
    };
    if params.is_none() {
        return Ok(Vec::new());
    }
    if params.is_instance_of::<PyDict>() {
        return Err(NotSupportedError::new_err(
            "named parameters are not supported: use ? placeholders and a sequence",
        ));
    }
    if params.is_instance_of::<PyString>() {
        return Err(ProgrammingError::new_err(
            "parameters are a sequence (a str is taken one character at a time: wrap it in a \
             tuple)",
        ));
    }
    params.try_iter()?.map(|v| to_value(&v?)).collect()
}

/// A connection to a database (PEP 249).
#[pyclass(module = "octopage._octopage")]
pub struct Connection {
    db: Db,
    conn: Mutex<Option<Conn>>,
    autocommit: AtomicBool,
    /// The open transaction was opened by this binding (not by an explicit `BEGIN`).
    implicit: AtomicBool,
    recovery: Mutex<Option<String>>,
}

type Outcome = (octopage::Outcome, i64);

impl Connection {
    /// Run one statement: in a transaction opened for it unless in autocommit mode or it only
    /// reads. Returns what it did and the last inserted rowid.
    fn run(&self, py: Python<'_>, sql: &str, params: Vec<Value>) -> PyResult<Outcome> {
        let begin = !self.autocommit.load(Ordering::Relaxed) && !reads_only(sql);
        py.detach(|| {
            let slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let conn = slot
                .as_ref()
                .ok_or_else(|| InterfaceError::new_err("the connection is closed"))?;
            each!(conn, c => {
                if begin && !c.in_transaction() {
                    c.execute("BEGIN", &[]).map_err(py_err)?;
                    self.implicit.store(true, Ordering::Relaxed);
                }
                let outcome = c.run_statement(sql, &params).map_err(py_err)?;
                if !c.in_transaction() {
                    self.implicit.store(false, Ordering::Relaxed);
                }
                Ok((outcome, c.last_insert_rowid()))
            })
        })
    }

    /// End the open transaction, if any, with `statement`.
    fn end(&self, py: Python<'_>, statement: &str) -> PyResult<()> {
        self.implicit.store(false, Ordering::Relaxed);
        py.detach(|| {
            let slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let Some(conn) = slot.as_ref() else {
                return Err(InterfaceError::new_err("the connection is closed"));
            };
            each!(conn, c => {
                if !c.in_transaction() {
                    return Ok(());
                }
                let result = c.execute(statement, &[]).map(|_| ());
                if result.is_err() && c.in_transaction() {
                    let _ = c.execute("ROLLBACK", &[]);
                }
                result.map_err(py_err)
            })
        })
    }
}

#[pymethods]
impl Connection {
    /// A new cursor.
    fn cursor(slf: Py<Self>) -> Cursor {
        Cursor::new(slf)
    }

    /// Run a statement on a new cursor, and return the cursor.
    #[pyo3(signature = (sql, parameters=None))]
    fn execute(
        slf: Py<Self>,
        py: Python<'_>,
        sql: &str,
        parameters: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Cursor> {
        let cursor = Cursor::new(slf);
        cursor.run(py, sql, parameters.as_ref())?;
        Ok(cursor)
    }

    /// Run a statement once for each set of parameters, on a new cursor.
    fn executemany(
        slf: Py<Self>,
        py: Python<'_>,
        sql: &str,
        seq_of_parameters: Bound<'_, PyAny>,
    ) -> PyResult<Cursor> {
        let cursor = Cursor::new(slf);
        cursor.run_many(py, sql, &seq_of_parameters)?;
        Ok(cursor)
    }

    /// Run several statements separated by semicolons, without parameters (as `sqlite3`'s
    /// `executescript`): a transaction this binding opened implicitly is committed first (one
    /// opened with an explicit `BEGIN` is left to the script), and the script's statements then
    /// run as they would one by one.
    fn executescript(&self, py: Python<'_>, sql: &str) -> PyResult<()> {
        if self.implicit.load(Ordering::Relaxed) {
            self.end(py, "COMMIT")?;
        }
        py.detach(|| {
            let slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let conn = slot
                .as_ref()
                .ok_or_else(|| InterfaceError::new_err("the connection is closed"))?;
            each!(conn, c => c.execute_batch(sql).map_err(py_err))
        })
    }

    /// Publish the open transaction as one commit. Raises `ConflictError` if another client
    /// changed the same data first; nothing was applied then.
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.end(py, "COMMIT")
    }

    /// Drop the open transaction.
    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        self.end(py, "ROLLBACK")
    }

    /// Close the connection, dropping an open transaction.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let _ = self.end(py, "ROLLBACK");
        py.detach(|| {
            self.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
        });
        Ok(())
    }

    /// Run statements — SQL strings, or `(sql, parameters)` pairs — as one transaction, and run
    /// them all again after a refused commit. Returns each statement's rows changed.
    fn run_transaction(&self, py: Python<'_>, statements: Bound<'_, PyAny>) -> PyResult<Vec<u64>> {
        let mut list = Vec::new();
        for item in statements.try_iter()? {
            let item = item?;
            if item.is_instance_of::<PyString>() {
                list.push(Statement::new(item.extract::<String>()?, &[]));
            } else {
                let (sql, values): (String, Bound<'_, PyAny>) = item.extract()?;
                list.push(Statement::new(sql, &params(Some(&values))?));
            }
        }
        py.detach(|| {
            let slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let conn = slot
                .as_ref()
                .ok_or_else(|| InterfaceError::new_err("the connection is closed"))?;
            each!(conn, c => {
                if c.in_transaction() {
                    return Err(ProgrammingError::new_err(
                        "commit or roll back the open transaction first",
                    ));
                }
                c.run_transaction(&list)
                    .map(|outcomes| outcomes.iter().map(|o| o.changed).collect())
                    .map_err(py_err)
            })
        })
    }

    /// Whether each statement commits by itself.
    #[getter]
    fn get_autocommit(&self) -> bool {
        self.autocommit.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_autocommit(&self, py: Python<'_>, value: bool) -> PyResult<()> {
        if value {
            self.end(py, "COMMIT")?;
        }
        self.autocommit.store(value, Ordering::Relaxed);
        Ok(())
    }

    /// Whether a transaction is open.
    #[getter]
    fn in_transaction(&self) -> bool {
        let slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(conn) => each!(conn, c => c.in_transaction()),
            None => false,
        }
    }

    /// The head commit (read now), as 40 hex digits.
    #[getter]
    fn head(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| {
            let head = runtime().block_on(async {
                match &self.db {
                    Db::Remote(d) => d.refresh().await,
                    Db::Memory(d) => d.refresh().await,
                }
            });
            head.map(|h| h.to_hex()).map_err(py_err)
        })
    }

    /// Where the database lives now: OWNER/NAME on GitHub (it changes when it moves to a new
    /// generation).
    #[getter]
    fn location(&self) -> Option<String> {
        match &self.db {
            Db::Remote(d) => d.location(),
            Db::Memory(d) => d.location(),
        }
    }

    /// The recovery key of a database this connection created encrypted: once, then None.
    #[getter]
    fn recovery_key(&self) -> Option<String> {
        self.recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Commit if the block succeeded, roll back if it raised.
    #[pyo3(signature = (exc_type=None, _exc=None, _traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if exc_type.is_some_and(|t| !t.is_none()) {
            self.rollback(py)?;
        } else {
            self.commit(py)?;
        }
        Ok(false)
    }
}

/// A cursor (PEP 249): runs statements and holds their rows.
#[pyclass(module = "octopage._octopage")]
pub struct Cursor {
    connection: Py<Connection>,
    rows: Mutex<std::collections::VecDeque<Vec<Value>>>,
    columns: Mutex<Option<Vec<String>>>,
    rowcount: AtomicI64,
    lastrowid: Mutex<Option<i64>>,
    arraysize: AtomicUsize,
    closed: AtomicBool,
}

impl Cursor {
    fn new(connection: Py<Connection>) -> Self {
        Cursor {
            connection,
            rows: Mutex::new(Default::default()),
            columns: Mutex::new(None),
            rowcount: AtomicI64::new(-1),
            lastrowid: Mutex::new(None),
            arraysize: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
        }
    }

    fn run(
        &self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(InterfaceError::new_err("the cursor is closed"));
        }
        let values = params(parameters)?;
        let connection = self.connection.bind(py).borrow();
        let (outcome, rowid) = connection.run(py, sql, values)?;
        let query = !outcome.rows.columns.is_empty();
        *self.columns.lock().unwrap() = query.then_some(outcome.rows.columns.clone());
        *self.rows.lock().unwrap() = outcome.rows.rows.into_iter().collect();
        self.rowcount.store(
            if query { -1 } else { outcome.changed as i64 },
            Ordering::Relaxed,
        );
        if outcome.changed > 0 && sql.trim_start().to_ascii_uppercase().starts_with("INSERT") {
            *self.lastrowid.lock().unwrap() = Some(rowid);
        }
        Ok(())
    }

    fn run_many(&self, py: Python<'_>, sql: &str, seq: &Bound<'_, PyAny>) -> PyResult<()> {
        let mut total = 0i64;
        for parameters in seq.try_iter()? {
            self.run(py, sql, Some(&parameters?))?;
            total += self.rowcount.load(Ordering::Relaxed).max(0);
        }
        self.rowcount.store(total, Ordering::Relaxed);
        Ok(())
    }

    fn next_row<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        let row = self.rows.lock().unwrap().pop_front();
        match row {
            Some(row) => {
                let items = row
                    .iter()
                    .map(|v| to_py(py, v))
                    .collect::<PyResult<Vec<_>>>()?;
                Ok(Some(PyTuple::new(py, items)?))
            }
            None => Ok(None),
        }
    }
}

#[pymethods]
impl Cursor {
    /// Run a statement.
    #[pyo3(signature = (sql, parameters=None))]
    fn execute<'py>(
        slf: PyRef<'py, Self>,
        py: Python<'py>,
        sql: &str,
        parameters: Option<Bound<'py, PyAny>>,
    ) -> PyResult<PyRef<'py, Self>> {
        slf.run(py, sql, parameters.as_ref())?;
        Ok(slf)
    }

    /// Run a statement once for each set of parameters.
    fn executemany<'py>(
        slf: PyRef<'py, Self>,
        py: Python<'py>,
        sql: &str,
        seq_of_parameters: Bound<'py, PyAny>,
    ) -> PyResult<PyRef<'py, Self>> {
        slf.run_many(py, sql, &seq_of_parameters)?;
        Ok(slf)
    }

    /// The next row, or None.
    fn fetchone<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        self.next_row(py)
    }

    /// The next `size` rows (default `arraysize`).
    #[pyo3(signature = (size=None))]
    fn fetchmany<'py>(&self, py: Python<'py>, size: Option<usize>) -> PyResult<Bound<'py, PyList>> {
        let size = size.unwrap_or_else(|| self.arraysize.load(Ordering::Relaxed));
        let list = PyList::empty(py);
        for _ in 0..size {
            match self.next_row(py)? {
                Some(row) => list.append(row)?,
                None => break,
            }
        }
        Ok(list)
    }

    /// All remaining rows.
    fn fetchall<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        while let Some(row) = self.next_row(py)? {
            list.append(row)?;
        }
        Ok(list)
    }

    /// The columns of the last query, as 7-item sequences (name, then six Nones), or None.
    #[getter]
    fn description<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        let columns = self.columns.lock().unwrap().clone();
        let Some(columns) = columns else {
            return Ok(None);
        };
        let items = columns
            .iter()
            .map(|name| {
                let none = || py.None().into_bound(py);
                PyTuple::new(
                    py,
                    [
                        name.into_pyobject(py)?.into_any(),
                        none(),
                        none(),
                        none(),
                        none(),
                        none(),
                        none(),
                    ],
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        Ok(Some(PyTuple::new(py, items)?))
    }

    /// Rows the last statement changed, or -1 after a query.
    #[getter]
    fn rowcount(&self) -> i64 {
        self.rowcount.load(Ordering::Relaxed)
    }

    /// The rowid of the row the last INSERT added.
    #[getter]
    fn lastrowid(&self) -> Option<i64> {
        *self.lastrowid.lock().unwrap()
    }

    #[getter]
    fn get_arraysize(&self) -> usize {
        self.arraysize.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_arraysize(&self, value: usize) {
        self.arraysize.store(value.max(1), Ordering::Relaxed);
    }

    #[getter]
    fn connection(&self, py: Python<'_>) -> Py<Connection> {
        self.connection.clone_ref(py)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.rows.lock().unwrap().clear();
    }

    #[pyo3(signature = (*_sizes))]
    fn setinputsizes(&self, _sizes: Bound<'_, PyTuple>) {}

    #[pyo3(signature = (_size, _column=None))]
    fn setoutputsize(&self, _size: Bound<'_, PyAny>, _column: Option<Bound<'_, PyAny>>) {}

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        self.next_row(py)?
            .ok_or_else(|| PyStopIteration::new_err(()))
    }
}

/// Open the database at `location`: "OWNER/NAME" on github.com, the URL of any smart-HTTP git
/// remote, or ":memory:" for a scratch database in this process. `token` signs in (a
/// fine-grained token or an app installation token).
#[pyfunction]
#[pyo3(signature = (
    location, token=None, *, branch="main", passphrase=None, recovery_key=None, create=false,
    encrypt=false, cache_dir=None, user=None, page_size=None, autocommit=false
))]
#[allow(clippy::too_many_arguments)]
fn connect(
    py: Python<'_>,
    location: &str,
    token: Option<String>,
    branch: &str,
    passphrase: Option<String>,
    recovery_key: Option<String>,
    create: bool,
    encrypt: bool,
    cache_dir: Option<String>,
    user: Option<String>,
    page_size: Option<usize>,
    autocommit: bool,
) -> PyResult<Connection> {
    let mut config = Config::default();
    config.store.branch = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    };
    config.store.cache_dir = cache_dir.map(Into::into);
    config.store.unlock = match (&recovery_key, &passphrase) {
        (Some(key), _) => Some(Unlock::RecoveryKey(key.clone())),
        (None, Some(p)) => Some(Unlock::Passphrase(p.clone())),
        (None, None) => None,
    };
    if let Some(size) = page_size {
        config.page_size = size;
    }
    if encrypt && passphrase.is_none() {
        return Err(ProgrammingError::new_err("encrypt=True needs a passphrase"));
    }
    let location = location.to_string();
    let user = user.unwrap_or_else(|| "x-access-token".into());
    let (db, conn, recovery) = py
        .detach(move || -> Result<(Db, Conn, Option<String>), OctoError> {
            // Opening is async; the SQLite connection is opened after, off the runtime, because
            // its VFS blocks on the runtime.
            let (db, recovery) = runtime().block_on(async {
                async fn open<T: octopage::Transport + 'static>(
                    make: impl Fn() -> octopage::Result<T>,
                    config: Config,
                    create: bool,
                    encryption: Option<String>,
                ) -> octopage::Result<(Database<T>, Option<String>)> {
                    match Database::open(make()?, config.clone()).await {
                        Err(OctoError::Store(octopage_pagestore::Error::NoDatabase(_)))
                            if create =>
                        {
                            match encryption {
                                None => Ok((Database::create(make()?, config).await?, None)),
                                Some(p) => {
                                    let (db, key) = Database::create_encrypted(
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
                let encryption = if encrypt { passphrase.clone() } else { None };
                if location == ":memory:" {
                    let (db, key) = open(|| Ok(InMemory::new()), config, true, encryption).await?;
                    return Ok::<_, OctoError>((Db::Memory(db), key));
                }
                let make = || {
                    if location.contains("://") {
                        octopage::remote(&location, token.as_deref(), &user)
                    } else {
                        octopage::github(&location, token.as_deref())
                    }
                };
                let (db, key) = open(make, config, create, encryption).await?;
                Ok((Db::Remote(db), key))
            })?;
            let conn = match &db {
                Db::Remote(d) => Conn::Remote(d.connect()?),
                Db::Memory(d) => Conn::Memory(d.connect()?),
            };
            Ok((db, conn, recovery))
        })
        .map_err(py_err)?;
    Ok(Connection {
        db,
        conn: Mutex::new(Some(conn)),
        autocommit: AtomicBool::new(autocommit),
        implicit: AtomicBool::new(false),
        recovery: Mutex::new(recovery),
    })
}

#[pymodule]
fn _octopage(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_function(wrap_pyfunction!(connect, m)?)?;
    m.add_class::<Connection>()?;
    m.add_class::<Cursor>()?;
    m.add("apilevel", "2.0")?;
    m.add("threadsafety", 1)?;
    m.add("paramstyle", "qmark")?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("Warning", py.get_type::<Warning>())?;
    m.add("Error", py.get_type::<Error>())?;
    m.add("InterfaceError", py.get_type::<InterfaceError>())?;
    m.add("DatabaseError", py.get_type::<DatabaseError>())?;
    m.add("DataError", py.get_type::<DataError>())?;
    m.add("OperationalError", py.get_type::<OperationalError>())?;
    m.add("IntegrityError", py.get_type::<IntegrityError>())?;
    m.add("InternalError", py.get_type::<InternalError>())?;
    m.add("ProgrammingError", py.get_type::<ProgrammingError>())?;
    m.add("NotSupportedError", py.get_type::<NotSupportedError>())?;
    m.add("ConflictError", py.get_type::<ConflictError>())?;
    m.add("UnavailableError", py.get_type::<UnavailableError>())?;
    m.add("OutcomeUnknownError", py.get_type::<OutcomeUnknownError>())?;
    m.add("FullError", py.get_type::<FullError>())?;
    m.add("LockedError", py.get_type::<LockedError>())?;
    Ok(())
}
