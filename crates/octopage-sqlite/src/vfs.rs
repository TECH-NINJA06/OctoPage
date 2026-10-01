#![allow(unsafe_code)]

use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use octopage_git::Transport;
use rusqlite::{Connection, ffi};

use crate::file::{Main, check_pragma};
use crate::{Blocked, Committed, Database};

struct VfsData<T: Transport + 'static> {
    io: ffi::sqlite3_io_methods,
    /// Databases waiting for SQLite to open them, by the file name `connect` passed.
    files: Mutex<HashMap<String, Arc<Database<T>>>>,
    name: CString,
}

// SAFETY: the io-methods table holds only function pointers; everything else is Sync.
unsafe impl<T: Transport + 'static> Sync for VfsData<T> {}
unsafe impl<T: Transport + 'static> Send for VfsData<T> {}

/// The VFS for transport type `T`, registered with SQLite on first use.
fn vfs_for<T: Transport + 'static>() -> &'static VfsData<T> {
    static REGISTERED: OnceLock<Mutex<HashMap<TypeId, usize>>> = OnceLock::new();
    let mut registered = REGISTERED.get_or_init(Default::default).lock().unwrap();
    if let Some(&address) = registered.get(&TypeId::of::<T>()) {
        return unsafe { &*(address as *const VfsData<T>) };
    }
    let data: &'static VfsData<T> = Box::leak(Box::new(VfsData {
        io: io_methods::<T>(),
        files: Mutex::new(HashMap::new()),
        name: CString::new(format!("octopage-{}", registered.len())).unwrap(),
    }));
    let vfs: &'static mut ffi::sqlite3_vfs = Box::leak(Box::new(ffi::sqlite3_vfs {
        iVersion: 2,
        szOsFile: size_of::<OctoFile<T>>() as c_int,
        mxPathname: 512,
        pNext: std::ptr::null_mut(),
        zName: data.name.as_ptr(),
        pAppData: data as *const VfsData<T> as *mut c_void,
        xOpen: Some(x_open::<T>),
        xDelete: Some(x_delete),
        xAccess: Some(x_access),
        xFullPathname: Some(x_full_pathname),
        xDlOpen: None,
        xDlError: None,
        xDlSym: None,
        xDlClose: None,
        xRandomness: Some(x_randomness),
        xSleep: Some(x_sleep),
        xCurrentTime: Some(x_current_time),
        xGetLastError: Some(x_get_last_error),
        xCurrentTimeInt64: Some(x_current_time_int64),
        xSetSystemCall: None,
        xGetSystemCall: None,
        xNextSystemCall: None,
    }));
    let rc = unsafe { ffi::sqlite3_vfs_register(vfs, 0) };
    assert_eq!(rc, ffi::SQLITE_OK, "registering the OctoPage VFS");
    registered.insert(TypeId::of::<T>(), data as *const VfsData<T> as usize);
    data
}

/// The name of the VFS for transport type `T`.
pub(crate) fn name<T: Transport + 'static>() -> &'static str {
    vfs_for::<T>().name.to_str().unwrap()
}

/// Hand `db` to the VFS under a fresh file name, for `Connection::open` to pick up.
pub(crate) fn register<T: Transport + 'static>(db: Arc<Database<T>>) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = format!("octopage-db-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    vfs_for::<T>()
        .files
        .lock()
        .unwrap()
        .insert(name.clone(), db);
    name
}

/// File-control operations of our own, for the layer above. SQLite passes unknown operations
/// through to the VFS; the argument is a pointer to a Rust value of the stated type.
const FCNTL_SET_CHANGELOG: c_int = 0x4f50_0001; // *mut Option<Vec<u8>>, taken
const FCNTL_LAST_COMMIT: c_int = 0x4f50_0002; // *mut Option<Committed>, filled in
const FCNTL_LAST_BLOCKED: c_int = 0x4f50_0003; // *mut Option<Blocked>, filled in

fn file_control<V>(conn: &Connection, op: c_int, value: &mut V) -> c_int {
    // SAFETY: the handle is valid for the call, and our xFileControl reads the argument as a
    // `V` for exactly these operations; another VFS answers SQLITE_NOTFOUND without touching it.
    unsafe {
        ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            op,
            value as *mut V as *mut c_void,
        )
    }
}

pub(crate) fn set_changelog(conn: &Connection, changelog: Option<Vec<u8>>) -> rusqlite::Result<()> {
    let mut value = changelog;
    match file_control(conn, FCNTL_SET_CHANGELOG, &mut value) {
        ffi::SQLITE_OK => Ok(()),
        rc => Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(rc),
            Some("not an OctoPage connection".into()),
        )),
    }
}

pub(crate) fn last_commit(conn: &Connection) -> Option<Committed> {
    let mut value: Option<Committed> = None;
    file_control(conn, FCNTL_LAST_COMMIT, &mut value);
    value
}

pub(crate) fn last_blocked(conn: &Connection) -> Option<Blocked> {
    let mut value: Option<Blocked> = None;
    file_control(conn, FCNTL_LAST_BLOCKED, &mut value);
    value
}

/// SQLite's own test for whether `sql` ends a complete statement (`sqlite3_complete`).
pub(crate) fn is_complete(sql: &str) -> bool {
    let Ok(text) = CString::new(sql) else {
        return false;
    };
    // SAFETY: a NUL-terminated string that outlives the call.
    unsafe { ffi::sqlite3_complete(text.as_ptr()) != 0 }
}

/// Set the bytes SQLite reserves at the end of each page (only takes effect on a new database).
pub(crate) fn reserve_bytes(conn: &Connection, bytes: usize) -> rusqlite::Result<()> {
    let mut reserve = bytes as c_int;
    // SAFETY: the connection handle is valid for the call; the argument is an int, as this
    // file-control expects.
    let rc = unsafe {
        ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_RESERVE_BYTES,
            &mut reserve as *mut c_int as *mut c_void,
        )
    };
    if rc != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(rc),
            Some("could not reserve bytes per page".into()),
        ));
    }
    Ok(())
}

fn io_methods<T: Transport + 'static>() -> ffi::sqlite3_io_methods {
    ffi::sqlite3_io_methods {
        iVersion: 1,
        xClose: Some(x_close::<T>),
        xRead: Some(x_read::<T>),
        xWrite: Some(x_write::<T>),
        xTruncate: Some(x_truncate::<T>),
        xSync: Some(x_sync::<T>),
        xFileSize: Some(x_file_size::<T>),
        xLock: Some(x_lock::<T>),
        xUnlock: Some(x_unlock::<T>),
        xCheckReservedLock: Some(x_check_reserved_lock),
        xFileControl: Some(x_file_control::<T>),
        xSectorSize: Some(x_sector_size),
        xDeviceCharacteristics: Some(x_device_characteristics),
        xShmMap: None,
        xShmLock: None,
        xShmBarrier: None,
        xShmUnmap: None,
        xFetch: None,
        xUnfetch: None,
    }
}

#[repr(C)]
struct OctoFile<T: Transport + 'static> {
    base: ffi::sqlite3_file,
    state: *mut FileState<T>,
}

enum FileState<T: Transport + 'static> {
    Main(Box<Main<T>>),
    /// Temporary files (sorts, statement journals, the temp database) live in memory.
    Temp(Vec<u8>),
}

unsafe fn state<'a, T: Transport + 'static>(file: *mut ffi::sqlite3_file) -> &'a mut FileState<T> {
    unsafe { &mut *(*(file as *mut OctoFile<T>)).state }
}

/// Run a callback body, turning a panic into an I/O error instead of unwinding into C.
fn guard(body: impl FnOnce() -> c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(ffi::SQLITE_IOERR)
}

fn code(result: Result<(), c_int>) -> c_int {
    match result {
        Ok(()) => ffi::SQLITE_OK,
        Err(rc) => rc,
    }
}

/// A string SQLite can free with `sqlite3_free`.
fn sqlite_string(text: &str) -> *mut c_char {
    let bytes = text.as_bytes();
    // SAFETY: the allocation is checked and holds the bytes plus a terminating NUL.
    unsafe {
        let out = ffi::sqlite3_malloc64(bytes.len() as u64 + 1) as *mut c_char;
        if !out.is_null() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, bytes.len());
            *out.add(bytes.len()) = 0;
        }
        out
    }
}

unsafe extern "C" fn x_open<T: Transport + 'static>(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    guard(|| unsafe {
        let data = &*((*vfs).pAppData as *const VfsData<T>);
        let file = file as *mut OctoFile<T>;
        (*file).base.pMethods = std::ptr::null();
        let state = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            if name.is_null() {
                return ffi::SQLITE_CANTOPEN;
            }
            let key = CStr::from_ptr(name).to_string_lossy().into_owned();
            // Only databases handed over by `Database::connect`: ATTACH or VACUUM INTO of
            // anything else is refused.
            let Some(db) = data.files.lock().unwrap().remove(&key) else {
                return ffi::SQLITE_CANTOPEN;
            };
            FileState::Main(Box::new(Main::new(db)))
        } else {
            FileState::Temp(Vec::new())
        };
        (*file).state = Box::into_raw(Box::new(state));
        (*file).base.pMethods = &data.io;
        if !out_flags.is_null() {
            *out_flags = flags;
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn x_close<T: Transport + 'static>(file: *mut ffi::sqlite3_file) -> c_int {
    guard(|| unsafe {
        let file = file as *mut OctoFile<T>;
        drop(Box::from_raw((*file).state));
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn x_read<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    guard(|| unsafe {
        let out = std::slice::from_raw_parts_mut(buf as *mut u8, amount as usize);
        out.fill(0);
        match state::<T>(file) {
            FileState::Temp(bytes) => {
                let start = (offset as usize).min(bytes.len());
                let end = (offset as usize + out.len()).min(bytes.len());
                out[..end - start].copy_from_slice(&bytes[start..end]);
                if end - start < out.len() {
                    ffi::SQLITE_IOERR_SHORT_READ
                } else {
                    ffi::SQLITE_OK
                }
            }
            FileState::Main(main) => main.read(out, offset as u64),
        }
    })
}

unsafe extern "C" fn x_write<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    buf: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    guard(|| unsafe {
        let data = std::slice::from_raw_parts(buf as *const u8, amount as usize);
        match state::<T>(file) {
            FileState::Temp(bytes) => {
                let end = offset as usize + data.len();
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[offset as usize..end].copy_from_slice(data);
                ffi::SQLITE_OK
            }
            FileState::Main(main) => code(main.write(data, offset as u64)),
        }
    })
}

unsafe extern "C" fn x_truncate<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    size: i64,
) -> c_int {
    guard(|| unsafe {
        match state::<T>(file) {
            FileState::Temp(bytes) => {
                bytes.truncate(size as usize);
                ffi::SQLITE_OK
            }
            FileState::Main(main) => code(main.truncate(size as u64)),
        }
    })
}

unsafe extern "C" fn x_sync<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    _flags: c_int,
) -> c_int {
    guard(|| unsafe {
        match state::<T>(file) {
            FileState::Temp(_) => ffi::SQLITE_OK,
            FileState::Main(main) => code(main.sync()),
        }
    })
}

unsafe extern "C" fn x_file_size<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    size: *mut i64,
) -> c_int {
    guard(|| unsafe {
        match state::<T>(file) {
            FileState::Temp(bytes) => {
                *size = bytes.len() as i64;
                ffi::SQLITE_OK
            }
            FileState::Main(main) => match main.size() {
                Ok(bytes) => {
                    *size = bytes as i64;
                    ffi::SQLITE_OK
                }
                Err(rc) => rc,
            },
        }
    })
}

unsafe extern "C" fn x_lock<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    level: c_int,
) -> c_int {
    guard(|| unsafe {
        match state::<T>(file) {
            FileState::Main(main) => code(main.lock(level)),
            FileState::Temp(_) => ffi::SQLITE_OK,
        }
    })
}

unsafe extern "C" fn x_unlock<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    level: c_int,
) -> c_int {
    guard(|| unsafe {
        if let FileState::Main(main) = state::<T>(file) {
            main.unlock(level);
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn x_check_reserved_lock(
    _file: *mut ffi::sqlite3_file,
    out: *mut c_int,
) -> c_int {
    unsafe { *out = 0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_file_control<T: Transport + 'static>(
    file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    guard(|| unsafe {
        let FileState::Main(main) = state::<T>(file) else {
            return ffi::SQLITE_NOTFOUND;
        };
        match op {
            FCNTL_SET_CHANGELOG => {
                main.set_changelog((*(arg as *mut Option<Vec<u8>>)).take());
                return ffi::SQLITE_OK;
            }
            FCNTL_LAST_COMMIT => {
                *(arg as *mut Option<Committed>) = main.last_commit();
                return ffi::SQLITE_OK;
            }
            FCNTL_LAST_BLOCKED => {
                *(arg as *mut Option<Blocked>) = main.last_blocked();
                return ffi::SQLITE_OK;
            }
            ffi::SQLITE_FCNTL_PRAGMA => {}
            _ => return ffi::SQLITE_NOTFOUND,
        }
        // arg is char*[3]: [0] an error message to return, [1] the pragma, [2] its value or NULL.
        let args = arg as *mut *mut c_char;
        let pragma = CStr::from_ptr(*args.add(1)).to_string_lossy();
        let value = (!(*args.add(2)).is_null())
            .then(|| CStr::from_ptr(*args.add(2)).to_string_lossy().into_owned());
        match check_pragma(
            &pragma,
            value.as_deref(),
            main.page_size(),
            main.max_pages(),
        ) {
            Ok(()) => ffi::SQLITE_NOTFOUND, // let SQLite handle it as usual
            Err(message) => {
                *args = sqlite_string(&message);
                ffi::SQLITE_ERROR
            }
        }
    })
}

unsafe extern "C" fn x_sector_size(_file: *mut ffi::sqlite3_file) -> c_int {
    crate::PAGE_SIZE as c_int
}

unsafe extern "C" fn x_device_characteristics(_file: *mut ffi::sqlite3_file) -> c_int {
    0
}

unsafe extern "C" fn x_delete(
    _vfs: *mut ffi::sqlite3_vfs,
    _name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_access(
    _vfs: *mut ffi::sqlite3_vfs,
    _name: *const c_char,
    _flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe { *out = 0 }; // no journals or side files exist
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    n_out: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let bytes = CStr::from_ptr(name).to_bytes();
        if bytes.len() + 1 > n_out as usize {
            return ffi::SQLITE_CANTOPEN;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, bytes.len());
        *out.add(bytes.len()) = 0;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_randomness(
    _vfs: *mut ffi::sqlite3_vfs,
    n: c_int,
    out: *mut c_char,
) -> c_int {
    let buf = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, n as usize) };
    fastrand::fill(buf);
    n
}

unsafe extern "C" fn x_sleep(_vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    std::thread::sleep(std::time::Duration::from_micros(micros as u64));
    micros
}

thread_local! {
    /// The time SQLite sees on this thread, when a transaction has fixed it (`with_clock`).
    static CLOCK: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Run `f` with SQLite's clock on this thread fixed at `unix_millis` (see `crate::with_clock`).
pub(crate) fn with_clock<R>(unix_millis: i64, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<i64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLOCK.with(|clock| clock.set(self.0));
        }
    }
    let _restore = Restore(CLOCK.with(|clock| clock.replace(Some(unix_millis))));
    f()
}

fn unix_millis() -> i64 {
    CLOCK.with(|clock| clock.get()).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

unsafe extern "C" fn x_current_time(_vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    unsafe { *out = 2_440_587.5 + unix_millis() as f64 / 86_400_000.0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_current_time_int64(_vfs: *mut ffi::sqlite3_vfs, out: *mut i64) -> c_int {
    unsafe { *out = 210_866_760_000_000 + unix_millis() };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_get_last_error(
    _vfs: *mut ffi::sqlite3_vfs,
    _n: c_int,
    _out: *mut c_char,
) -> c_int {
    0
}
