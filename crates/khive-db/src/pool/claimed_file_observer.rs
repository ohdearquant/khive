//! Observe the actual descriptor opened by the bundled Unix SQLite VFS.
//!
//! This observes `fstat`, rather than combining two pathname observations. It
//! does not prevent native open-time I/O: SQLite may write to a zero-size file
//! on macOS msdos/exfat, and trusted autoextensions run before open returns.

use crate::error::SqliteError;
use crate::file_identity::DatabaseFileIdentity;
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::OnceLock;

#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
))]
type Fstat = unsafe extern "C" fn(libc::c_int, *mut libc::stat) -> libc::c_int;

static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
))]
static ORIGINAL: OnceLock<Fstat> = OnceLock::new();

#[derive(Clone, Copy)]
struct Observation {
    expected: (u64, u64),
    count: u64,
    mismatch: Option<(u64, u64)>,
    #[cfg(test)]
    hook_failed: bool,
}

thread_local! {
    static OPEN: Cell<Option<Observation>> = const { Cell::new(None) };
}

#[cfg(test)]
type TestHook = Box<dyn FnOnce((u64, u64))>;

#[cfg(test)]
thread_local! {
    static TEST_HOOK: std::cell::RefCell<Option<TestHook>> = const { std::cell::RefCell::new(None) };
}

/// One test-only callback after the first successful fstat in an active open.
/// It sees the recorded actual descriptor identity and never owns the fd.
#[cfg(test)]
pub(super) fn set_test_hook(hook: Option<TestHook>) {
    TEST_HOOK.with(|current| *current.borrow_mut() = hook);
}

/// Install one immutable forwarding observer for the locked bundled Unix VFS.
/// Later calls return the recorded result without changing its syscall table.
///
/// # Safety
///
/// The first call must occur at process startup, before any thread performs
/// SQLite file I/O. SQLite's syscall setter is not synchronized with VFS I/O;
/// a lazy first-claimed-open call does not satisfy this requirement. The
/// process must retain its trusted bundled Unix VFS and nonmutating open-time
/// autoextensions, and must not subsequently replace its `fstat` syscall.
pub unsafe fn initialize() -> Result<(), SqliteError> {
    INITIALIZED
        .get_or_init(|| {
            // SAFETY: the caller establishes the startup boundary above.
            unsafe { install() }
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| SqliteError::InvalidData(error.clone()))
}

#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
))]
unsafe fn install() -> Result<(), String> {
    use rusqlite::ffi;

    // SAFETY: initialization owns the pre-I/O startup boundary; VFS pointers
    // and their method tables are retained by SQLite for the process lifetime.
    let (vfs, unix) = unsafe {
        (
            ffi::sqlite3_vfs_find(std::ptr::null()),
            ffi::sqlite3_vfs_find(c"unix".as_ptr()),
        )
    };
    if vfs.is_null() || vfs != unix {
        return Err("claimed-file observation requires the default bundled Unix VFS".into());
    }
    // SAFETY: the nonnull VFS is live and no concurrent VFS mutation is allowed.
    if unsafe { ffi::sqlite3_libversion_number() } != ffi::SQLITE_VERSION_NUMBER
        || unsafe { (*vfs).iVersion } < 3
    {
        return Err("claimed-file observation requires the locked SQLite VFS version".into());
    }
    // SAFETY: iVersion >= 3 exposes these optional public syscall methods.
    let (get, set) = unsafe { ((*vfs).xGetSystemCall, (*vfs).xSetSystemCall) };
    let get = get.ok_or("SQLite VFS does not expose its fstat syscall")?;
    let set = set.ok_or("SQLite VFS does not expose its syscall setter")?;
    // SAFETY: the bundled Unix VFS documents the fstat entry's native ABI as
    // int(int, struct stat*). The supported 64-bit platform libc matches it.
    let erased = unsafe { get(vfs, c"fstat".as_ptr()) }.ok_or("SQLite VFS has no fstat syscall")?;
    let original = unsafe { std::mem::transmute::<unsafe extern "C" fn(), Fstat>(erased) };
    if !std::ptr::fn_addr_eq(original, libc::fstat as Fstat) {
        return Err("SQLite fstat syscall was already replaced or has an unknown ABI".into());
    }
    ORIGINAL
        .set(original)
        .map_err(|_| "SQLite fstat observer was already initialized")?;
    // SAFETY: only the public erased representation changes; callback uses the
    // same native fstat ABI and remains installed for the process lifetime.
    let observer = unsafe { std::mem::transmute::<Fstat, unsafe extern "C" fn()>(observe_fstat) };
    let result = unsafe { set(vfs, c"fstat".as_ptr(), Some(observer)) };
    if result != ffi::SQLITE_OK {
        return Err(format!(
            "cannot install SQLite fstat observer (result {result})"
        ));
    }
    let installed = unsafe { get(vfs, c"fstat".as_ptr()) };
    if !installed.is_some_and(|callback| std::ptr::fn_addr_eq(callback, observer)) {
        return Err("SQLite VFS did not retain the installed fstat observer".into());
    }
    Ok(())
}

#[cfg(not(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
)))]
unsafe fn install() -> Result<(), String> {
    Err("claimed-file observation supports the verified 64-bit Linux and macOS fstat ABI".into())
}

/// A single-threaded synchronous main-file open observation.
/// Nested guards refuse rather than mixing observations from different opens.
pub(super) struct Guard {
    previous: Option<Observation>,
    _same_thread: PhantomData<Rc<()>>,
}

pub(super) fn begin(expected: DatabaseFileIdentity) -> Result<Guard, SqliteError> {
    if !matches!(INITIALIZED.get(), Some(Ok(()))) {
        return Err(SqliteError::InvalidData(
            "SQLite claimed-file observer was not initialized at process startup".into(),
        ));
    }
    OPEN.with(|open| {
        let previous = open.get();
        if previous.is_some() {
            return Err(SqliteError::InvalidData(
                "nested SQLite claimed-file open observation is unsupported".into(),
            ));
        }
        open.set(Some(Observation {
            expected: expected.unix_parts(),
            count: 0,
            mismatch: None,
            #[cfg(test)]
            hook_failed: false,
        }));
        Ok(Guard {
            previous,
            _same_thread: PhantomData,
        })
    })
}

impl Guard {
    /// Check immediately after SQLite open, before any SQL or pragmas. Every
    /// successful fstat must identify the held claim; absent evidence refuses.
    pub(super) fn finish(self) -> Result<(), SqliteError> {
        OPEN.with(|open| match open.get() {
            #[cfg(test)]
            Some(Observation {
                hook_failed: true, ..
            }) => Err(SqliteError::InvalidData(
                "SQLite descriptor observation test hook failed".into(),
            )),
            Some(Observation {
                mismatch: Some(actual),
                expected,
                ..
            }) => Err(SqliteError::InvalidData(format!(
                "opened SQLite descriptor {actual:?} differs from held claim {expected:?}"
            ))),
            Some(Observation { count, .. }) if count > 0 => Ok(()),
            _ => Err(SqliteError::InvalidData(
                "SQLite open produced no actual descriptor identity observation".into(),
            )),
        })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // No panic while unwinding or during thread-local destruction.
        let _ = OPEN.try_with(|open| open.set(self.previous));
    }
}

#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
))]
unsafe extern "C" fn observe_fstat(fd: libc::c_int, buffer: *mut libc::stat) -> libc::c_int {
    // The callback is installed only after ORIGINAL is set. The fallback is
    // native forwarding as well; neither branch takes descriptor ownership.
    let original = ORIGINAL.get().copied().unwrap_or(libc::fstat);
    // SAFETY: preserve the caller's native fstat arguments and return value.
    let result = unsafe { original(fd, buffer) };
    if result == 0 && !buffer.is_null() {
        // SAFETY: successful native fstat initialized the caller's valid stat
        // buffer. Preserve errno even if platform TLS initialization touches it.
        let saved_errno = unsafe { *errno_pointer() };
        // Native stat field widths and signedness differ across these ABIs.
        #[allow(clippy::unnecessary_cast)]
        let identity = unsafe { ((*buffer).st_dev as u64, (*buffer).st_ino as u64) };
        let active = OPEN.try_with(|open| {
            if let Some(mut observation) = open.get() {
                observation.count = observation.count.saturating_add(1);
                if identity != observation.expected && observation.mismatch.is_none() {
                    observation.mismatch = Some(identity);
                }
                open.set(Some(observation));
                true
            } else {
                false
            }
        });
        #[cfg(test)]
        if matches!(active, Ok(true)) {
            // Take before invoking: a test rename or nested native observation
            // cannot execute this one-shot callback again or borrow recursively.
            let hook = TEST_HOOK
                .try_with(|current| {
                    current
                        .try_borrow_mut()
                        .ok()
                        .and_then(|mut hook| hook.take())
                })
                .ok()
                .flatten();
            if let Some(hook) = hook {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(identity)));
                if result.is_err() {
                    let _ = OPEN.try_with(|open| {
                        if let Some(mut observation) = open.get() {
                            observation.hook_failed = true;
                            open.set(Some(observation));
                        }
                    });
                }
            }
        }
        #[cfg(not(test))]
        let _ = active;
        unsafe { *errno_pointer() = saved_errno };
    }
    result
}

#[cfg(all(target_pointer_width = "64", target_os = "linux"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(all(target_pointer_width = "64", target_os = "macos"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
