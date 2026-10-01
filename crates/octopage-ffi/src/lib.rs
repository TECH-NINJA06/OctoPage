#![allow(unsafe_code)] // a C ABI is raw pointers; every entry point checks them before use
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)] // the safety rules are the conventions above, and the header

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, OnceLock};

use octopage::{
    Config, Connection, Database, Error, InMemory, SmartHttp, Statement, Unlock, Value,
};

/// A status code: `OCTO_OK`, or what kind of failure (see `octo_last_error()` for the message).
pub type OctoStatus = i32;

/// Success.
pub const OCTO_OK: OctoStatus = 0;
/// A failure without a more specific code.
pub const OCTO_ERROR: OctoStatus = 1;
/// SQLite refused the statement: syntax, a constraint, a type.
pub const OCTO_SQL: OctoStatus = 2;
/// The commit was refused because another client changed the same data first. Nothing was
/// applied; run the transaction again.
pub const OCTO_CONFLICT: OctoStatus = 3;
/// The transaction lost too many races in a row to other writers; try again later.
pub const OCTO_BUSY: OctoStatus = 4;
/// The repository could not be reached, or throttled the request. Nothing was committed; try
/// again later.
pub const OCTO_UNAVAILABLE: OctoStatus = 5;
/// The network failed while committing: whether the commit landed is unknown. Check before
/// running it again.
pub const OCTO_OUTCOME_UNKNOWN: OctoStatus = 6;
/// The write would grow the database past its live-size limit.
pub const OCTO_FULL: OctoStatus = 7;
/// The database is encrypted: open it with its passphrase (or recovery key), which this was not.
pub const OCTO_LOCKED: OctoStatus = 8;
/// There is no database on that branch, or no such repository.
pub const OCTO_NOT_FOUND: OctoStatus = 9;
/// An argument was missing or malformed.
pub const OCTO_INVALID: OctoStatus = 10;
/// GitHub's push protection refused the commit: something in it looks like a secret.
pub const OCTO_SECRET_BLOCKED: OctoStatus = 11;

/// `octo_open` flag: create the database if the branch has none.
pub const OCTO_OPEN_CREATE: u32 = 1;
/// `octo_open` flag: when creating, create it encrypted (the `passphrase` option is required);
/// its recovery key is then available once from `octo_database_recovery_key`.
pub const OCTO_OPEN_ENCRYPT: u32 = 2;

/// `OctoValue` type: SQL NULL.
pub const OCTO_NULL: u32 = 0;
/// `OctoValue` type: a 64-bit signed integer, in `integer`.
pub const OCTO_INTEGER: u32 = 1;
/// `OctoValue` type: a 64-bit float, in `real`.
pub const OCTO_REAL: u32 = 2;
/// `OctoValue` type: UTF-8 text, `len` bytes at `bytes` (OctoPage's text is also followed by a
/// NUL, not counted in `len`).
pub const OCTO_TEXT: u32 = 3;
/// `OctoValue` type: a blob, `len` bytes at `bytes`.
pub const OCTO_BLOB: u32 = 4;

/// A SQL value: a parameter going in, or a result coming out. The pointer in a result value
/// stays valid until its `OctoRows` is freed.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OctoValue {
    /// `OCTO_NULL`, `OCTO_INTEGER`, `OCTO_REAL`, `OCTO_TEXT` or `OCTO_BLOB`.
    pub kind: u32,
    /// The value of an `OCTO_INTEGER`.
    pub integer: i64,
    /// The value of an `OCTO_REAL`.
    pub real: f64,
    /// The bytes of an `OCTO_TEXT` or `OCTO_BLOB` (may be NULL when `len` is 0).
    pub bytes: *const u8,
    /// How many bytes.
    pub len: usize,
}

/// Options for `octo_open`, set as key and value strings (`octo_options_set`).
pub struct OctoOptions {
    branch: Option<String>,
    passphrase: Option<String>,
    recovery_key: Option<String>,
    cache_dir: Option<String>,
    user: Option<String>,
    page_size: Option<usize>,
}

enum Db {
    Remote(Database<SmartHttp>),
    Memory(Database<InMemory>),
}

/// An open database.
pub struct OctoDatabase {
    db: Db,
    recovery: Mutex<Option<String>>,
}

enum Conn {
    Remote(Connection<SmartHttp>),
    Memory(Connection<InMemory>),
}

/// A connection: statements, queries and transactions.
pub struct OctoConnection {
    conn: Conn,
}

struct Cell {
    value: Value,
    /// Text with a NUL after it, or a blob's bytes.
    bytes: Vec<u8>,
}

/// A query's result.
pub struct OctoRows {
    columns: Vec<CString>,
    rows: Vec<Vec<Cell>>,
}

macro_rules! each_db {
    ($db:expr, $d:ident => $body:expr) => {
        match $db {
            Db::Remote($d) => $body,
            Db::Memory($d) => $body,
        }
    };
}

macro_rules! each_conn {
    ($conn:expr, $c:ident => $body:expr) => {
        match $conn {
            Conn::Remote($c) => $body,
            Conn::Memory($c) => $body,
        }
    };
}

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

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

type Failure = (OctoStatus, String);

fn set_last_error(message: &str) {
    let message = CString::new(message.replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|last| *last.borrow_mut() = message);
}

fn status_of(error: &Error) -> OctoStatus {
    use octopage_pagestore::Error as Store;
    match error {
        Error::Sql(_) => OCTO_SQL,
        Error::Conflict => OCTO_CONFLICT,
        Error::Serialization { .. } => OCTO_BUSY,
        Error::Unavailable(_) => OCTO_UNAVAILABLE,
        Error::OutcomeUnknown => OCTO_OUTCOME_UNKNOWN,
        Error::Full(_) => OCTO_FULL,
        Error::SecretBlocked { .. } => OCTO_SECRET_BLOCKED,
        Error::Store(Store::Locked | Store::WrongKey) => OCTO_LOCKED,
        Error::Store(Store::NoDatabase(_)) => OCTO_NOT_FOUND,
        Error::Store(Store::Transport(e)) if matches!(**e, octopage_git::Error::NotFound(_)) => {
            OCTO_NOT_FOUND
        }
        Error::Transport(octopage_git::Error::NotFound(_)) => OCTO_NOT_FOUND,
        Error::Invalid(_) | Error::AsOf { .. } => OCTO_INVALID,
        _ => OCTO_ERROR,
    }
}

fn fail(error: Error) -> Failure {
    (status_of(&error), error.to_string())
}

fn invalid(message: impl Into<String>) -> Failure {
    (OCTO_INVALID, message.into())
}

/// Run an entry point's body: record a failure's message, and never let a panic reach C.
fn call(body: impl FnOnce() -> Result<(), Failure>) -> OctoStatus {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => {
            set_last_error("");
            OCTO_OK
        }
        Ok(Err((status, message))) => {
            set_last_error(&message);
            status
        }
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            set_last_error(&format!("OctoPage failed internally: {what}"));
            OCTO_ERROR
        }
    }
}

/// A required string argument.
unsafe fn text<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, Failure> {
    if ptr.is_null() {
        return Err(invalid(format!("{what} is NULL")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| invalid(format!("{what} is not UTF-8")))
}

/// An optional string argument.
unsafe fn maybe_text<'a>(ptr: *const c_char, what: &str) -> Result<Option<&'a str>, Failure> {
    if ptr.is_null() {
        return Ok(None);
    }
    unsafe { text(ptr, what) }.map(Some)
}

unsafe fn params(params: *const OctoValue, count: usize) -> Result<Vec<Value>, Failure> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if params.is_null() {
        return Err(invalid("params is NULL but count is not 0"));
    }
    let values = unsafe { std::slice::from_raw_parts(params, count) };
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let bytes = || -> Result<&[u8], Failure> {
                match (v.bytes.is_null(), v.len) {
                    (_, 0) => Ok(&[]),
                    (true, _) => Err(invalid(format!("parameter {} has no bytes", i + 1))),
                    (false, len) => Ok(unsafe { std::slice::from_raw_parts(v.bytes, len) }),
                }
            };
            Ok(match v.kind {
                OCTO_NULL => Value::Null,
                OCTO_INTEGER => Value::Integer(v.integer),
                OCTO_REAL => Value::Real(v.real),
                OCTO_TEXT => Value::Text(
                    String::from_utf8(bytes()?.to_vec())
                        .map_err(|_| invalid(format!("parameter {} is not UTF-8", i + 1)))?,
                ),
                OCTO_BLOB => Value::Blob(bytes()?.to_vec()),
                other => {
                    return Err(invalid(format!(
                        "parameter {}: unknown type {other}",
                        i + 1
                    )));
                }
            })
        })
        .collect()
}

unsafe fn put<T>(ptr: *mut T, value: T, what: &str) -> Result<(), Failure> {
    if ptr.is_null() {
        return Err(invalid(format!("{what} is NULL")));
    }
    unsafe { ptr.write(value) };
    Ok(())
}

/// Write a 40-digit commit id and a NUL into `out` (41 bytes).
unsafe fn write_id(out: *mut c_char, id: octopage::ObjectId) -> Result<(), Failure> {
    if out.is_null() {
        return Err(invalid("the output buffer is NULL"));
    }
    let hex = id.to_hex();
    let buffer = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, 41) };
    buffer[..40].copy_from_slice(hex.as_bytes());
    buffer[40] = 0;
    Ok(())
}

// ------------------------------------------------------------------------------ the library

/// OctoPage's version, such as "0.1.0". Belongs to OctoPage.
#[unsafe(no_mangle)]
pub extern "C" fn octo_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// The message of the last failure on this thread ("" after a success). It stays valid until
/// the next OctoPage call on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn octo_last_error() -> *const c_char {
    LAST_ERROR.with(|last| last.borrow().as_ptr())
}

/// Free a string OctoPage returned as `char *`. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_string_free(string: *mut c_char) {
    if !string.is_null() {
        drop(unsafe { CString::from_raw(string) });
    }
}

// ------------------------------------------------------------------------------ options

/// New options, all unset.
#[unsafe(no_mangle)]
pub extern "C" fn octo_options_new() -> *mut OctoOptions {
    Box::into_raw(Box::new(OctoOptions {
        branch: None,
        passphrase: None,
        recovery_key: None,
        cache_dir: None,
        user: None,
        page_size: None,
    }))
}

/// Set an option: "branch" (the ref, default "refs/heads/main"), "passphrase",
/// "recovery_key", "cache_dir" (keep fetched pages there between runs), "user" (the user name
/// for a URL remote, default "x-access-token") or "page_size" (for a new database: 4096, 8192
/// or 16384). A NULL value unsets it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_options_set(
    options: *mut OctoOptions,
    key: *const c_char,
    value: *const c_char,
) -> OctoStatus {
    call(|| {
        let options = unsafe { options.as_mut() }.ok_or_else(|| invalid("options is NULL"))?;
        let key = unsafe { text(key, "key") }?;
        let value = unsafe { maybe_text(value, "value") }?.map(str::to_string);
        match key {
            "branch" => options.branch = value,
            "passphrase" => options.passphrase = value,
            "recovery_key" => options.recovery_key = value,
            "cache_dir" => options.cache_dir = value,
            "user" => options.user = value,
            "page_size" => {
                options.page_size = match value {
                    None => None,
                    Some(v) => Some(v.parse().map_err(|_| invalid("page_size is a number"))?),
                }
            }
            other => return Err(invalid(format!("unknown option {other:?}"))),
        }
        Ok(())
    })
}

/// Free options. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_options_free(options: *mut OctoOptions) {
    if !options.is_null() {
        drop(unsafe { Box::from_raw(options) });
    }
}

// ------------------------------------------------------------------------------ databases

fn config(options: Option<&OctoOptions>) -> Config {
    let mut config = Config::default();
    let Some(o) = options else { return config };
    if let Some(branch) = &o.branch {
        config.store.branch = branch.clone();
    }
    config.store.cache_dir = o.cache_dir.as_ref().map(Into::into);
    config.store.unlock = match (&o.recovery_key, &o.passphrase) {
        (Some(key), _) => Some(Unlock::RecoveryKey(key.clone())),
        (None, Some(passphrase)) => Some(Unlock::Passphrase(passphrase.clone())),
        (None, None) => None,
    };
    if let Some(size) = o.page_size {
        config.page_size = size;
    }
    config
}

async fn open_or_create<T: octopage::Transport + 'static>(
    make: impl Fn() -> octopage::Result<T>,
    config: Config,
    options: Option<&OctoOptions>,
    flags: u32,
) -> Result<(Database<T>, Option<String>), Failure> {
    match Database::open(make().map_err(fail)?, config.clone()).await {
        Ok(db) => Ok((db, None)),
        Err(Error::Store(octopage_pagestore::Error::NoDatabase(_)))
            if flags & OCTO_OPEN_CREATE != 0 =>
        {
            if flags & OCTO_OPEN_ENCRYPT == 0 {
                let db = Database::create(make().map_err(fail)?, config)
                    .await
                    .map_err(fail)?;
                return Ok((db, None));
            }
            let passphrase = options
                .and_then(|o| o.passphrase.clone())
                .ok_or_else(|| invalid("OCTO_OPEN_ENCRYPT needs the passphrase option"))?;
            let (db, recovery) = Database::create_encrypted(
                make().map_err(fail)?,
                config,
                octopage::Encryption::passphrase(passphrase),
            )
            .await
            .map_err(fail)?;
            Ok((db, Some(recovery.to_string())))
        }
        Err(e) => Err(fail(e)),
    }
}

/// Open the database at `location`: "OWNER/NAME" on github.com, the URL of any smart-HTTP git
/// remote, or ":memory:" for a scratch database that lives in this process. `token` signs in
/// (NULL: anonymous). `options` may be NULL. `flags`: `OCTO_OPEN_CREATE`, `OCTO_OPEN_ENCRYPT`.
/// On success `*out` is the database; free it with `octo_database_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_open(
    location: *const c_char,
    token: *const c_char,
    options: *const OctoOptions,
    flags: u32,
    out: *mut *mut OctoDatabase,
) -> OctoStatus {
    call(|| {
        let location = unsafe { text(location, "location") }?;
        let token = unsafe { maybe_text(token, "token") }?;
        let options = unsafe { options.as_ref() };
        let config = config(options);
        let (db, recovery) = runtime().block_on(async {
            if location == ":memory:" {
                let (db, recovery) = open_or_create(
                    || Ok(InMemory::new()),
                    config,
                    options,
                    flags | OCTO_OPEN_CREATE,
                )
                .await?;
                return Ok((Db::Memory(db), recovery));
            }
            let user = options
                .and_then(|o| o.user.clone())
                .unwrap_or_else(|| "x-access-token".into());
            let make = || {
                if location.contains("://") {
                    octopage::remote(location, token, &user)
                } else {
                    octopage::github(location, token)
                }
            };
            let (db, recovery) = open_or_create(make, config, options, flags).await?;
            Ok::<_, Failure>((Db::Remote(db), recovery))
        })?;
        let handle = Box::new(OctoDatabase {
            db,
            recovery: Mutex::new(recovery),
        });
        unsafe { put(out, Box::into_raw(handle), "out") }
    })
}

/// The recovery key of a database this handle just created encrypted, once: a string to free
/// with `octo_string_free`, or NULL. Store it: it opens the database if the passphrase is lost.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_database_recovery_key(db: *const OctoDatabase) -> *mut c_char {
    let Some(db) = (unsafe { db.as_ref() }) else {
        return std::ptr::null_mut();
    };
    match db.recovery.lock().unwrap_or_else(|e| e.into_inner()).take() {
        Some(key) => CString::new(key).map_or(std::ptr::null_mut(), CString::into_raw),
        None => std::ptr::null_mut(),
    }
}

/// Read the head now, and write its commit id (40 hex digits and a NUL) into `out`, 41 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_database_head(
    db: *const OctoDatabase,
    out: *mut c_char,
) -> OctoStatus {
    call(|| {
        let db = unsafe { db.as_ref() }.ok_or_else(|| invalid("db is NULL"))?;
        let head = runtime()
            .block_on(async { each_db!(&db.db, d => d.refresh().await) })
            .map_err(fail)?;
        unsafe { write_id(out, head) }
    })
}

/// Where the database lives now (it can move to a new generation): a string to free with
/// `octo_string_free`, or NULL if the transport cannot say.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_database_location(db: *const OctoDatabase) -> *mut c_char {
    let Some(db) = (unsafe { db.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let location = each_db!(&db.db, d => d.location());
    location
        .and_then(|l| CString::new(l).ok())
        .map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Close a database. Connections to it must be freed first. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_database_free(db: *mut OctoDatabase) {
    if !db.is_null() {
        let db = unsafe { Box::from_raw(db) };
        let _guard = runtime().enter(); // its background tasks stop on the runtime
        drop(db);
    }
}

// ------------------------------------------------------------------------------ connections

/// Open a connection. Free it with `octo_connection_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_connect(
    db: *const OctoDatabase,
    out: *mut *mut OctoConnection,
) -> OctoStatus {
    call(|| {
        let db = unsafe { db.as_ref() }.ok_or_else(|| invalid("db is NULL"))?;
        let conn = match &db.db {
            Db::Remote(d) => Conn::Remote(d.connect().map_err(fail)?),
            Db::Memory(d) => Conn::Memory(d.connect().map_err(fail)?),
        };
        unsafe { put(out, Box::into_raw(Box::new(OctoConnection { conn })), "out") }
    })
}

/// Close a connection (rolling back a transaction left open). NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_connection_free(conn: *mut OctoConnection) {
    if !conn.is_null() {
        drop(unsafe { Box::from_raw(conn) });
    }
}

/// Run one statement with `count` parameters (`params` may be NULL when `count` is 0). Outside
/// `BEGIN … COMMIT` it commits on its own, and runs again by itself after a refused commit.
/// `changed` (may be NULL) receives the rows it inserted, updated or deleted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_execute(
    conn: *mut OctoConnection,
    sql: *const c_char,
    params: *const OctoValue,
    count: usize,
    changed: *mut u64,
) -> OctoStatus {
    call(|| {
        let conn = unsafe { conn.as_ref() }.ok_or_else(|| invalid("conn is NULL"))?;
        let sql = unsafe { text(sql, "sql") }?;
        let params = unsafe { self::params(params, count) }?;
        let n = each_conn!(&conn.conn, c => c.execute(sql, &params)).map_err(fail)?;
        if !changed.is_null() {
            unsafe { changed.write(n) };
        }
        Ok(())
    })
}

/// Run several statements separated by semicolons, without parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_execute_batch(
    conn: *mut OctoConnection,
    sql: *const c_char,
) -> OctoStatus {
    call(|| {
        let conn = unsafe { conn.as_ref() }.ok_or_else(|| invalid("conn is NULL"))?;
        let sql = unsafe { text(sql, "sql") }?;
        each_conn!(&conn.conn, c => c.execute_batch(sql)).map_err(fail)
    })
}

/// Run a query (a statement that returns rows; `… AS OF '<commit or time>'` reads the past).
/// On success `*out` holds the rows; free them with `octo_rows_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_query(
    conn: *mut OctoConnection,
    sql: *const c_char,
    params: *const OctoValue,
    count: usize,
    out: *mut *mut OctoRows,
) -> OctoStatus {
    call(|| {
        let conn = unsafe { conn.as_ref() }.ok_or_else(|| invalid("conn is NULL"))?;
        let sql = unsafe { text(sql, "sql") }?;
        let params = unsafe { self::params(params, count) }?;
        let rows = each_conn!(&conn.conn, c => c.query(sql, &params)).map_err(fail)?;
        let columns = rows
            .columns
            .iter()
            .map(|c| CString::new(c.as_str()).unwrap_or_default())
            .collect();
        let rows = rows
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| {
                        let bytes = match &value {
                            Value::Text(t) => {
                                let mut bytes = t.as_bytes().to_vec();
                                bytes.push(0);
                                bytes
                            }
                            Value::Blob(b) => b.clone(),
                            _ => Vec::new(),
                        };
                        Cell { value, bytes }
                    })
                    .collect()
            })
            .collect();
        let handle = Box::new(OctoRows { columns, rows });
        unsafe { put(out, Box::into_raw(handle), "out") }
    })
}

/// Run `count` statements as one transaction, running them all again after a refused commit
/// (the way to write when others write too). Statements take no parameters here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_run_transaction(
    conn: *mut OctoConnection,
    statements: *const *const c_char,
    count: usize,
) -> OctoStatus {
    call(|| {
        let conn = unsafe { conn.as_ref() }.ok_or_else(|| invalid("conn is NULL"))?;
        if statements.is_null() && count > 0 {
            return Err(invalid("statements is NULL"));
        }
        let list = if count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(statements, count) }
        };
        let statements = list
            .iter()
            .map(|s| unsafe { text(*s, "a statement") }.map(|sql| Statement::new(sql, &[])))
            .collect::<Result<Vec<_>, _>>()?;
        each_conn!(&conn.conn, c => c.run_transaction(&statements))
            .map(|_| ())
            .map_err(fail)
    })
}

/// Whether the connection is inside `BEGIN … COMMIT`: 1 or 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_in_transaction(conn: *const OctoConnection) -> c_int {
    match unsafe { conn.as_ref() } {
        Some(conn) => each_conn!(&conn.conn, c => c.in_transaction()) as c_int,
        None => 0,
    }
}

/// The commit this connection's last commit made (40 hex digits and a NUL into `out`, 41
/// bytes). `OCTO_NOT_FOUND` if it has not committed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_last_commit(
    conn: *const OctoConnection,
    out: *mut c_char,
) -> OctoStatus {
    call(|| {
        let conn = unsafe { conn.as_ref() }.ok_or_else(|| invalid("conn is NULL"))?;
        let last = each_conn!(&conn.conn, c => c.last_commit()).ok_or((
            OCTO_NOT_FOUND,
            "this connection has not committed".to_string(),
        ))?;
        unsafe { write_id(out, last.head) }
    })
}

// ------------------------------------------------------------------------------ rows

/// How many rows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_rows_count(rows: *const OctoRows) -> usize {
    unsafe { rows.as_ref() }.map_or(0, |r| r.rows.len())
}

/// How many columns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_rows_columns(rows: *const OctoRows) -> usize {
    unsafe { rows.as_ref() }.map_or(0, |r| r.columns.len())
}

/// Column `column`'s name, or NULL. Belongs to the rows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_rows_column_name(
    rows: *const OctoRows,
    column: usize,
) -> *const c_char {
    unsafe { rows.as_ref() }
        .and_then(|r| r.columns.get(column))
        .map_or(std::ptr::null(), |c| c.as_ptr())
}

/// The value at `row` and `column`. Its bytes belong to the rows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_rows_value(
    rows: *const OctoRows,
    row: usize,
    column: usize,
    out: *mut OctoValue,
) -> OctoStatus {
    call(|| {
        let rows = unsafe { rows.as_ref() }.ok_or_else(|| invalid("rows is NULL"))?;
        let cell = rows
            .rows
            .get(row)
            .and_then(|r| r.get(column))
            .ok_or_else(|| invalid(format!("no value at row {row}, column {column}")))?;
        let mut value = OctoValue {
            kind: OCTO_NULL,
            integer: 0,
            real: 0.0,
            bytes: std::ptr::null(),
            len: 0,
        };
        match &cell.value {
            Value::Null => {}
            Value::Integer(i) => {
                value.kind = OCTO_INTEGER;
                value.integer = *i;
            }
            Value::Real(f) => {
                value.kind = OCTO_REAL;
                value.real = *f;
            }
            Value::Text(_) => {
                value.kind = OCTO_TEXT;
                value.bytes = cell.bytes.as_ptr();
                value.len = cell.bytes.len() - 1; // not the NUL
            }
            Value::Blob(_) => {
                value.kind = OCTO_BLOB;
                value.bytes = cell.bytes.as_ptr();
                value.len = cell.bytes.len();
            }
        }
        unsafe { put(out, value, "out") }
    })
}

/// Free rows. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn octo_rows_free(rows: *mut OctoRows) {
    if !rows.is_null() {
        drop(unsafe { Box::from_raw(rows) });
    }
}
