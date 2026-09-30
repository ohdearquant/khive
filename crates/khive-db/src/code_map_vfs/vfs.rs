//! SQLite VFS entry points. File opens never delegate to the platform VFS.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::ffi;

use super::{callbacks, CodeMapHandleGuard, GuardError, Mode, OpenAccess, ProductionKind, Role};

const REGISTRATION_CAP: usize = 512;

#[derive(Hash, PartialEq, Eq)]
struct RollbackKey {
    target: PathBuf,
    protected: Vec<(PathBuf, ProductionKind)>,
}

#[derive(Default)]
struct Registrations {
    rollback: HashMap<RollbackKey, RollbackEntry>,
    total: usize,
}

struct RollbackEntry {
    name: String,
    guard: Arc<CodeMapHandleGuard>,
}

fn registrations() -> &'static Mutex<Registrations> {
    static REGISTRATIONS: OnceLock<Mutex<Registrations>> = OnceLock::new();
    REGISTRATIONS.get_or_init(|| Mutex::new(Registrations::default()))
}

#[repr(C)]
struct Registration {
    vfs: ffi::sqlite3_vfs,
    _name: CString,
    guard: Arc<CodeMapHandleGuard>,
}

// A registered VFS must outlive every SQLite connection that selected it.
// SQLite's global registry and these allocations intentionally live for the
// process lifetime, including after a code-map pool has been dropped.
pub(super) fn register(guard: Arc<CodeMapHandleGuard>) -> Result<String, GuardError> {
    // A code.ingest runtime may be constructed on every request. Reuse the
    // pinned rollback guard for the same exact target/protected contract;
    // transition guards remain one-shot and never enter this map.
    let mut registrations = registrations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (guard.mode == Mode::Rollback).then(|| RollbackKey {
        target: guard.target.clone(),
        protected: guard
            .protected
            .iter()
            .map(|base| (base.path.clone(), base.kind))
            .collect(),
    });
    if let Some(key) = &key {
        if let Some(existing) = registrations.rollback.get(key) {
            if existing.guard.parent.identity() != guard.parent.identity() {
                return Err(GuardError::Unsafe {
                    path: guard.target.clone(),
                    reason: "code-map parent changed while its VFS registration remains live",
                });
            }
            existing.guard.preflight()?;
            return Ok(existing.name.clone());
        }
    }
    if registrations.total >= REGISTRATION_CAP {
        return Err(GuardError::RegistrationFull);
    }
    // SAFETY: sqlite3_initialize and sqlite3_vfs_find are process-global
    // SQLite APIs; the returned default VFS remains registered for this call.
    let initialized = unsafe { ffi::sqlite3_initialize() };
    if initialized != ffi::SQLITE_OK {
        return Err(GuardError::Unsafe {
            path: guard.target.clone(),
            reason: "SQLite initialization failed before guarded VFS registration",
        });
    }
    // SAFETY: the bundled SQLite library owns its default VFS.
    let native = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
    if native.is_null() {
        return Err(GuardError::Unsafe {
            path: guard.target.clone(),
            reason: "SQLite has no platform VFS for non-file services",
        });
    }
    static NEXT_NAME: AtomicU64 = AtomicU64::new(1);
    let display_name = format!(
        "khive-code-map-{}",
        NEXT_NAME.fetch_add(1, Ordering::Relaxed)
    );
    let name = CString::new(display_name.as_bytes()).expect("generated VFS name has no NUL");
    // SAFETY: sqlite3_vfs is Copy and native is valid. The copied callbacks
    // provide only non-file services: every file-operation entry is replaced.
    let mut vfs = unsafe { *native };
    vfs.szOsFile = callbacks::os_file_size();
    vfs.pNext = ptr::null_mut();
    vfs.zName = name.as_ptr();
    vfs.xOpen = Some(open);
    vfs.xDelete = Some(delete);
    vfs.xAccess = Some(access);
    vfs.xFullPathname = Some(full_pathname);
    let mut registration = Box::new(Registration {
        vfs,
        _name: name,
        guard,
    });
    // SAFETY: registration is retained after success, so the pointer and
    // zName remain valid for SQLite's registry. makeDflt=0 changes no
    // ordinary database's VFS selection.
    if unsafe { ffi::sqlite3_vfs_register(&raw mut registration.vfs, 0) } != ffi::SQLITE_OK {
        return Err(GuardError::Unsafe {
            path: registration.guard.target.clone(),
            reason: "SQLite refused guarded VFS registration",
        });
    }
    let registration = Box::leak(registration);
    registrations.total += 1;
    if let Some(key) = key {
        registrations.rollback.insert(
            key,
            RollbackEntry {
                name: display_name.clone(),
                guard: Arc::clone(&registration.guard),
            },
        );
    }
    Ok(display_name)
}

#[cfg(any(test, feature = "test-support"))]
/// Take the refusal the guard registered as `name` last recorded, if any.
pub(super) fn take_refusal(name: &str) -> Option<String> {
    let guard = registrations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .rollback
        .values()
        .find(|entry| entry.name == name)
        .map(|entry| Arc::clone(&entry.guard))?;
    guard.take_refusal()
}

pub(super) fn registration_count() -> usize {
    registrations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .total
}

// SAFETY: p_vfs points to the leading `vfs` member of a leaked Registration.
unsafe fn registration<'a>(p_vfs: *mut ffi::sqlite3_vfs) -> &'a Registration {
    unsafe { &*p_vfs.cast::<Registration>() }
}

fn path_from_name(z_name: *const c_char) -> Option<PathBuf> {
    if z_name.is_null() {
        return None;
    }
    // SAFETY: SQLite supplies a NUL-terminated pathname to each VFS callback.
    let name = unsafe { CStr::from_ptr(z_name) };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes())))
    }
    #[cfg(windows)]
    {
        name.to_str().ok().map(PathBuf::from)
    }
}

fn role_for_path(guard: &CodeMapHandleGuard, path: &Path) -> Option<Role> {
    if path == guard.path(Role::Main) {
        Some(Role::Main)
    } else if path == guard.path(Role::Journal) {
        Some(Role::Journal)
    } else if path == guard.path(Role::TransitionWal) {
        Some(Role::TransitionWal)
    } else if path == guard.path(Role::Shm) {
        Some(Role::Shm)
    } else {
        None
    }
}

fn role_from_flags(flags: c_int) -> Option<Role> {
    if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
        Some(Role::Main)
    } else if flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0 {
        Some(Role::Journal)
    } else if flags & ffi::SQLITE_OPEN_WAL != 0 {
        Some(Role::TransitionWal)
    } else {
        None
    }
}

fn open_access(flags: c_int) -> Option<OpenAccess> {
    if flags & ffi::SQLITE_OPEN_READONLY != 0 {
        if flags & (ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE) != 0 {
            return None;
        }
        Some(OpenAccess::ReadOnly)
    } else if flags & ffi::SQLITE_OPEN_READWRITE != 0 {
        if flags & ffi::SQLITE_OPEN_CREATE == 0 {
            Some(OpenAccess::ReadWrite)
        } else if flags & ffi::SQLITE_OPEN_EXCLUSIVE != 0 {
            Some(OpenAccess::CreateNew)
        } else {
            Some(OpenAccess::Create)
        }
    } else {
        None
    }
}

unsafe extern "C" fn open(
    p_vfs: *mut ffi::sqlite3_vfs,
    z_name: ffi::sqlite3_filename,
    p_file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    if p_file.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: SQLite allocates at least szOsFile bytes before calling xOpen.
    unsafe { callbacks::prepare_open(p_file) };
    // SAFETY: the registered VFS pointer refers to a leaked Registration.
    let registration = unsafe { registration(p_vfs) };
    let Some(path) = path_from_name(z_name) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let (Some(path_role), Some(flag_role), Some(access)) = (
        role_for_path(&registration.guard, &path),
        role_from_flags(flags),
        open_access(flags),
    ) else {
        return ffi::SQLITE_CANTOPEN;
    };
    if path_role != flag_role
        || path_role == Role::Shm
        || (path_role == Role::Main && flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0)
    {
        return ffi::SQLITE_CANTOPEN;
    }
    let handle = match registration.guard.open(path_role, access) {
        Ok(handle) => handle,
        Err(error) => {
            registration.guard.record_refusal(path_role, &error);
            return ffi::SQLITE_CANTOPEN;
        }
    };
    if !out_flags.is_null() {
        // SAFETY: SQLite supplies an optional writable output pointer.
        unsafe { *out_flags = flags & (ffi::SQLITE_OPEN_READONLY | ffi::SQLITE_OPEN_READWRITE) };
    }
    // SAFETY: prepare_open initialized the slot, the retained handle passed
    // admission, and no fallible work follows publishing pMethods.
    unsafe {
        callbacks::install(
            p_file,
            handle,
            Arc::clone(&registration.guard),
            flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0,
        );
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn access(
    p_vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    if out.is_null()
        || !matches!(
            flags,
            ffi::SQLITE_ACCESS_EXISTS | ffi::SQLITE_ACCESS_READ | ffi::SQLITE_ACCESS_READWRITE
        )
    {
        return ffi::SQLITE_IOERR_ACCESS;
    }
    // SAFETY: registered VFS pointer is a live Registration.
    let guard = &unsafe { registration(p_vfs) }.guard;
    let Some(role) = path_from_name(z_name).and_then(|path| role_for_path(guard, &path)) else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    let Ok(exists) = guard.access(role) else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    // SAFETY: out is non-null and owned by SQLite for this callback.
    unsafe { *out = c_int::from(exists) };
    ffi::SQLITE_OK
}

unsafe extern "C" fn delete(
    p_vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    sync_dir: c_int,
) -> c_int {
    // SAFETY: registered VFS pointer is a live Registration.
    let guard = &unsafe { registration(p_vfs) }.guard;
    let Some(role) = path_from_name(z_name).and_then(|path| role_for_path(guard, &path)) else {
        return ffi::SQLITE_IOERR_DELETE;
    };
    match guard.delete(role, sync_dir != 0) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_IOERR_DELETE,
    }
}

unsafe extern "C" fn full_pathname(
    p_vfs: *mut ffi::sqlite3_vfs,
    z_name: *const c_char,
    out_len: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: registered VFS pointer is a live Registration.
    let guard = &unsafe { registration(p_vfs) }.guard;
    let Some(path) = path_from_name(z_name) else {
        return ffi::SQLITE_CANTOPEN;
    };
    if role_for_path(guard, &path).is_none() || out.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    // xFullPathname preserves the admitted absolute spelling; it must not
    // canonicalize an explicit target through a symlinked parent.
    // SAFETY: SQLite supplied z_name as a NUL-terminated pathname.
    let bytes = unsafe { CStr::from_ptr(z_name) }.to_bytes_with_nul();
    if out_len <= 0 || bytes.len() > out_len as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: checked length fits SQLite's writable output buffer.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len()) };
    ffi::SQLITE_OK
}
