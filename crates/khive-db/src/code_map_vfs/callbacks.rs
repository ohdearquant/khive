//! SQLite file callbacks over the native code-map VFS's attested handle.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs::File;
use std::io;
use std::mem::{size_of, ManuallyDrop};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::ffi;

use super::{os, CodeMapHandleGuard, GuardedFile, Mode, Role};

#[repr(C)]
struct CallbackFile {
    base: ffi::sqlite3_file,
    handle: ManuallyDrop<GuardedFile>,
    guard: Arc<CodeMapHandleGuard>,
    rollback_lock: Mutex<os::RollbackLock>,
    sqlite_lock_level: AtomicI32,
    lock_poisoned: AtomicBool,
    delete_on_close: bool,
    shm_violation: AtomicBool,
}

static SHM_VIOLATIONS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn os_file_size() -> c_int {
    c_int::try_from(size_of::<CallbackFile>()).expect("SQLite file object size fits c_int")
}

pub(super) fn shm_violation_count() -> usize {
    SHM_VIOLATIONS.load(Ordering::Relaxed)
}

/// # Safety
/// `p_file` must be a writable SQLite allocation at least `os_file_size()`
/// bytes long, aligned for `CallbackFile`. Call this before any fallible work
/// in VFS `xOpen`, so SQLite cannot call `xClose` on a failed open.
pub(super) unsafe fn prepare_open(p_file: *mut ffi::sqlite3_file) {
    // SAFETY: the caller guarantees that the leading sqlite3_file is writable.
    unsafe { (*p_file).pMethods = ptr::null() };
}

/// # Safety
/// `p_file` must meet `prepare_open`'s allocation requirements and must not
/// already contain a live `CallbackFile`. `handle` must have passed the native
/// guard's opened-handle identity check before this method publishes it.
pub(super) unsafe fn install(
    p_file: *mut ffi::sqlite3_file,
    handle: GuardedFile,
    guard: Arc<CodeMapHandleGuard>,
    delete_on_close: bool,
) {
    let rollback_lock = Mutex::new(os::RollbackLock::new(handle.identity));
    // SAFETY: the caller owns a sufficiently sized, aligned SQLite file slot.
    unsafe {
        p_file.cast::<CallbackFile>().write(CallbackFile {
            base: ffi::sqlite3_file {
                pMethods: ptr::null(),
            },
            handle: ManuallyDrop::new(handle),
            guard,
            rollback_lock,
            sqlite_lock_level: AtomicI32::new(ffi::SQLITE_LOCK_NONE),
            lock_poisoned: AtomicBool::new(false),
            delete_on_close,
            shm_violation: AtomicBool::new(false),
        });
        (*p_file).pMethods = &METHODS;
    }
}

// SAFETY: the VFS must allocate `os_file_size()` bytes and call `install`
// before SQLite invokes a method. CallbackFile starts with sqlite3_file.
unsafe fn file<'a>(p_file: *mut ffi::sqlite3_file) -> &'a CallbackFile {
    // SAFETY: the caller has a live, installed CallbackFile.
    unsafe { &*p_file.cast::<CallbackFile>() }
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
}

fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_write(buf, offset)
    }
}

fn checked_io_range(amount: c_int, offset: ffi::sqlite3_int64) -> Option<(usize, u64)> {
    let amount = usize::try_from(amount).ok()?;
    let offset = u64::try_from(offset).ok()?;
    if offset.checked_add(amount as u64)? > i64::MAX as u64 {
        return None;
    }
    Some((amount, offset))
}

unsafe extern "C" fn close(p_file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: SQLite calls xClose only for an installed CallbackFile. The
    // value is moved out exactly once; clearing pMethods disables re-close.
    let opened = unsafe {
        (*p_file).pMethods = ptr::null();
        ptr::read(p_file.cast::<CallbackFile>())
    };
    let CallbackFile {
        base: _,
        handle,
        guard,
        rollback_lock,
        sqlite_lock_level,
        lock_poisoned: _,
        delete_on_close,
        shm_violation: _,
    } = opened;
    let mut handle = ManuallyDrop::into_inner(handle);
    let delete_result = if delete_on_close {
        guard.delete(handle.role, false)
    } else {
        Ok(())
    };
    let file = handle.take_file();
    let close_result = if handle.role == Role::Main {
        rollback_lock
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close(file)
    } else {
        os::close_unlocked(file, handle.identity);
        ffi::SQLITE_OK
    };
    if handle.role == Role::Main
        && sqlite_lock_level.load(Ordering::Acquire) == ffi::SQLITE_LOCK_EXCLUSIVE
    {
        guard.exclusive_main_handles.fetch_sub(1, Ordering::AcqRel);
    }
    drop(handle);
    if delete_result.is_err() {
        ffi::SQLITE_IOERR_DELETE
    } else {
        close_result
    }
}

unsafe extern "C" fn read(
    p_file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.shm_violation.load(Ordering::Relaxed) || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR_READ;
    }
    let Some((amount, offset)) = checked_io_range(amount, offset) else {
        return ffi::SQLITE_IOERR_READ;
    };
    if amount == 0 {
        return ffi::SQLITE_OK;
    }
    if buffer.is_null() {
        return ffi::SQLITE_IOERR_READ;
    }
    // SAFETY: SQLite supplies `amount` writable bytes for xRead.
    let buffer = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), amount) };
    let mut done = 0;
    while done < amount {
        let Some(at) = offset.checked_add(done as u64) else {
            return ffi::SQLITE_IOERR_READ;
        };
        match read_at(opened.handle.file(), &mut buffer[done..], at) {
            Ok(0) => {
                buffer[done..].fill(0);
                return ffi::SQLITE_IOERR_SHORT_READ;
            }
            Ok(count) => done += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return ffi::SQLITE_IOERR_READ,
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn write(
    p_file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.shm_violation.load(Ordering::Relaxed) || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR_WRITE;
    }
    let Some((amount, offset)) = checked_io_range(amount, offset) else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    if amount == 0 {
        return ffi::SQLITE_OK;
    }
    if buffer.is_null() {
        return ffi::SQLITE_IOERR_WRITE;
    }
    // SAFETY: SQLite supplies `amount` readable bytes for xWrite.
    let buffer = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), amount) };
    let mut done = 0;
    while done < amount {
        let Some(at) = offset.checked_add(done as u64) else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        match write_at(opened.handle.file(), &buffer[done..], at) {
            Ok(0) => return ffi::SQLITE_IOERR_WRITE,
            Ok(count) => done += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return ffi::SQLITE_IOERR_WRITE,
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn truncate(p_file: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.shm_violation.load(Ordering::Relaxed) || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR_TRUNCATE;
    }
    let Ok(size) = u64::try_from(size) else {
        return ffi::SQLITE_IOERR_TRUNCATE;
    };
    match opened.handle.file().set_len(size) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_IOERR_TRUNCATE,
    }
}

unsafe extern "C" fn sync(p_file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.shm_violation.load(Ordering::Relaxed) || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR_FSYNC;
    }
    #[cfg(target_os = "macos")]
    if flags & 0x0f == ffi::SQLITE_SYNC_FULL {
        use std::os::fd::AsRawFd;
        // SAFETY: the attested file descriptor remains live for this callback.
        if unsafe { libc::fcntl(opened.handle.file().as_raw_fd(), libc::F_FULLFSYNC, 0) } == 0 {
            return ffi::SQLITE_OK;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = flags;
    match opened.handle.file().sync_all() {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_IOERR_FSYNC,
    }
}

unsafe extern "C" fn file_size(
    p_file: *mut ffi::sqlite3_file,
    result: *mut ffi::sqlite3_int64,
) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.shm_violation.load(Ordering::Relaxed)
        || opened.lock_poisoned.load(Ordering::Acquire)
        || result.is_null()
    {
        return ffi::SQLITE_IOERR_FSTAT;
    }
    let Ok(metadata) = opened.handle.file().metadata() else {
        return ffi::SQLITE_IOERR_FSTAT;
    };
    let Ok(size) = ffi::sqlite3_int64::try_from(metadata.len()) else {
        return ffi::SQLITE_IOERR_FSTAT;
    };
    // SAFETY: SQLite supplies a writable output pointer for xFileSize.
    unsafe { *result = size };
    ffi::SQLITE_OK
}

unsafe extern "C" fn lock(p_file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.handle.role != Role::Main
        || opened.shm_violation.load(Ordering::Relaxed)
        || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR_LOCK;
    }
    let result = opened
        .rollback_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lock(opened.handle.file(), level);
    if result == ffi::SQLITE_OK && level == ffi::SQLITE_LOCK_EXCLUSIVE {
        let previous = opened
            .sqlite_lock_level
            .swap(ffi::SQLITE_LOCK_EXCLUSIVE, Ordering::AcqRel);
        if previous != ffi::SQLITE_LOCK_EXCLUSIVE {
            opened
                .guard
                .exclusive_main_handles
                .fetch_add(1, Ordering::AcqRel);
        }
    } else if result != ffi::SQLITE_OK && result != ffi::SQLITE_BUSY {
        opened.lock_poisoned.store(true, Ordering::Release);
        if opened
            .sqlite_lock_level
            .swap(ffi::SQLITE_LOCK_NONE, Ordering::AcqRel)
            == ffi::SQLITE_LOCK_EXCLUSIVE
        {
            opened
                .guard
                .exclusive_main_handles
                .fetch_sub(1, Ordering::AcqRel);
        }
    }
    result
}

unsafe extern "C" fn unlock(p_file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.handle.role != Role::Main || opened.lock_poisoned.load(Ordering::Acquire) {
        return ffi::SQLITE_IOERR_UNLOCK;
    }
    let result = opened
        .rollback_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unlock(opened.handle.file(), level);
    if result == ffi::SQLITE_OK {
        let previous = opened.sqlite_lock_level.swap(level, Ordering::AcqRel);
        if previous == ffi::SQLITE_LOCK_EXCLUSIVE {
            opened
                .guard
                .exclusive_main_handles
                .fetch_sub(1, Ordering::AcqRel);
        }
    } else {
        opened.lock_poisoned.store(true, Ordering::Release);
        if opened
            .sqlite_lock_level
            .swap(ffi::SQLITE_LOCK_NONE, Ordering::AcqRel)
            == ffi::SQLITE_LOCK_EXCLUSIVE
        {
            opened
                .guard
                .exclusive_main_handles
                .fetch_sub(1, Ordering::AcqRel);
        }
    }
    result
}

unsafe extern "C" fn check_reserved_lock(
    p_file: *mut ffi::sqlite3_file,
    result: *mut c_int,
) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.handle.role != Role::Main
        || result.is_null()
        || opened.shm_violation.load(Ordering::Relaxed)
        || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
    }
    // SAFETY: SQLite supplies a writable output pointer.
    let result = unsafe { &mut *result };
    opened
        .rollback_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .check_reserved(opened.handle.file(), result)
}

unsafe extern "C" fn file_control(
    p_file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    // SAFETY: SQLite passes an installed file object.
    let opened = unsafe { file(p_file) };
    if opened.shm_violation.load(Ordering::Relaxed) || opened.lock_poisoned.load(Ordering::Acquire)
    {
        return ffi::SQLITE_IOERR;
    }
    if op == ffi::SQLITE_FCNTL_PERSIST_WAL
        && opened.handle.role == Role::Main
        && opened.guard.mode == Mode::QuiescentWalTransition
    {
        if arg.is_null() {
            return ffi::SQLITE_IOERR;
        }
        // SQLite's last-WAL-close path queries this control with -1 and
        // unlinks the WAL unless it reads 1. A refusal before the mode-switch
        // PRAGMA must leave the previously attested sidecar names intact.
        // SAFETY: SQLITE_FCNTL_PERSIST_WAL carries a writable int pointer.
        unsafe { *arg.cast::<c_int>() = 1 };
        return ffi::SQLITE_OK;
    }
    if op == ffi::SQLITE_FCNTL_PRAGMA && !arg.is_null() && opened.handle.role == Role::Main {
        // SQLITE_FCNTL_PRAGMA receives char *argv[]: argv[1] is the name,
        // argv[2] is NULL for a read or the requested value for a write.
        // Refuse mode changes before SQLite compiles any statement that
        // might create WAL/SHM or disable durable rollback journaling.
        // SAFETY: SQLite owns an array of at least three string pointers.
        let args = arg.cast::<*mut c_char>();
        let name = unsafe { *args.add(1) };
        let value = unsafe { *args.add(2) };
        if !name.is_null() && !value.is_null() {
            // SAFETY: SQLite supplies NUL-terminated PRAGMA name/value.
            let name = unsafe { CStr::from_ptr(name) };
            let value = unsafe { CStr::from_ptr(value) };
            if name.to_bytes().eq_ignore_ascii_case(b"journal_mode")
                && (!value.to_bytes().eq_ignore_ascii_case(b"delete")
                    || !opened.guard.consume_mode_switch_authorization())
            {
                return ffi::SQLITE_ERROR;
            }
        }
    }
    #[cfg(unix)]
    {
        if op == ffi::SQLITE_FCNTL_HAS_MOVED && opened.handle.role == Role::Main {
            if arg.is_null() {
                return ffi::SQLITE_IOERR;
            }
            // Callers verify that the opened main is still the file at its
            // path; answering NOTFOUND would leave that question unanswerable.
            let moved = !os::path_names(&opened.guard.path(Role::Main), opened.handle.identity);
            // SAFETY: SQLITE_FCNTL_HAS_MOVED carries a writable int pointer.
            unsafe { *arg.cast::<c_int>() = c_int::from(moved) };
            return ffi::SQLITE_OK;
        }
    }
    ffi::SQLITE_NOTFOUND
}

unsafe extern "C" fn sector_size(_p_file: *mut ffi::sqlite3_file) -> c_int {
    4096
}

unsafe extern "C" fn device_characteristics(_p_file: *mut ffi::sqlite3_file) -> c_int {
    0
}

// SAFETY: SQLite passes an installed file object. All shared-memory methods
// mark that file poisoned so the void xShmBarrier cannot silently resume I/O.
unsafe fn record_shm_violation(p_file: *mut ffi::sqlite3_file) {
    SHM_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: SQLite passes an installed CallbackFile to each xShm method.
    unsafe { file(p_file) }
        .shm_violation
        .store(true, Ordering::Relaxed);
}

unsafe extern "C" fn shm_map(
    p_file: *mut ffi::sqlite3_file,
    _page: c_int,
    _page_size: c_int,
    _is_write: c_int,
    result: *mut *mut c_void,
) -> c_int {
    if !result.is_null() {
        // SAFETY: SQLite supplies a writable output pointer when non-null.
        unsafe { *result = ptr::null_mut() };
    }
    // SAFETY: SQLite passes an installed CallbackFile.
    unsafe { record_shm_violation(p_file) };
    ffi::SQLITE_IOERR_SHMMAP
}

unsafe extern "C" fn shm_lock(
    p_file: *mut ffi::sqlite3_file,
    _offset: c_int,
    _count: c_int,
    _flags: c_int,
) -> c_int {
    // SAFETY: SQLite passes an installed CallbackFile.
    unsafe { record_shm_violation(p_file) };
    ffi::SQLITE_IOERR_SHMLOCK
}

unsafe extern "C" fn shm_barrier(p_file: *mut ffi::sqlite3_file) {
    // SAFETY: SQLite passes an installed CallbackFile.
    unsafe { record_shm_violation(p_file) };
}

unsafe extern "C" fn shm_unmap(p_file: *mut ffi::sqlite3_file, _delete: c_int) -> c_int {
    // SAFETY: SQLite passes an installed CallbackFile.
    unsafe { record_shm_violation(p_file) };
    ffi::SQLITE_IOERR_SHMOPEN
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(check_reserved_lock),
    xFileControl: Some(file_control),
    xSectorSize: Some(sector_size),
    xDeviceCharacteristics: Some(device_characteristics),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: None,
    xUnfetch: None,
};
