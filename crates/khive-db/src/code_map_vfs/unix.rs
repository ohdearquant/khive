use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use rusqlite::ffi;

use super::{Observed, OpenAccess};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Identity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

pub(super) struct PinnedParent {
    directory: File,
    identity: Identity,
}

fn invalid_path() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "non-plain code-map path")
}

fn name_cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| invalid_path())
}

fn fstat(fd: libc::c_int) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fd is live and the output buffer is valid for libc::fstat.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the entire stat structure on success.
    Ok(unsafe { stat.assume_init() })
}

fn identity(stat: &libc::stat) -> Identity {
    Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

/// Whether `path` still names the file with `opened`, the question SQLite's
/// Unix VFS answers for SQLITE_FCNTL_HAS_MOVED. The path is not followed, as
/// the guard never admits a symlinked member.
pub(super) fn path_names(path: &Path, opened: Identity) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: path is NUL-terminated and the output buffer is valid for lstat.
    if unsafe { libc::lstat(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: lstat initialized the entire stat structure on success.
    identity(&unsafe { stat.assume_init() }) == opened
}

fn regular_observation(stat: &libc::stat) -> io::Result<Observed> {
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "code-map member is not a regular file or is a symlink",
        ));
    }
    if stat.st_nlink == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "code-map member has no directory link",
        ));
    }
    // nlink_t is u16 on macOS and u64 on Linux, so the widening is a no-op on
    // one of them.
    #[allow(clippy::useless_conversion)]
    let links = u64::from(stat.st_nlink);
    Ok(Observed {
        identity: identity(stat),
        links,
    })
}

impl PinnedParent {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid_path());
        }
        let root = CString::new("/").expect("constant root path");
        // SAFETY: root is NUL-terminated; a successful fd is uniquely owned
        // by the File constructed directly below.
        let root_fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: root_fd was returned by the successful open above.
        let mut directory = unsafe { File::from_raw_fd(root_fd) };
        let mut traversed = PathBuf::from("/");
        for component in path.components() {
            let Component::Normal(name) = component else {
                if matches!(component, Component::RootDir) {
                    continue;
                }
                return Err(invalid_path());
            };
            let component_path = traversed.join(name);
            let name = name_cstring(name)?;
            // SAFETY: directory is live, name is NUL-terminated, and the
            // returned fd is immediately owned by a File below.
            let next_fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if next_fd < 0 {
                let error = io::Error::last_os_error();
                if error
                    .raw_os_error()
                    .is_some_and(|code| code == libc::ENOTDIR || code == libc::ELOOP)
                {
                    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                    // SAFETY: the current parent fd and component name are live;
                    // this diagnostic observation does not follow a symlink.
                    if unsafe {
                        libc::fstatat(
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            stat.as_mut_ptr(),
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } == 0
                        // SAFETY: successful fstatat initialized the stat result.
                        && unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "symlinked code-map parent component {}",
                                component_path.display()
                            ),
                        ));
                    }
                }
                return Err(error);
            }
            // SAFETY: next_fd was returned by the successful openat above.
            directory = unsafe { File::from_raw_fd(next_fd) };
            traversed = component_path;
        }
        let stat = fstat(directory.as_raw_fd())?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "code-map parent is not a directory",
            ));
        }
        Ok(Self {
            directory,
            identity: identity(&stat),
        })
    }

    pub(super) fn identity(&self) -> Identity {
        self.identity
    }

    pub(super) fn stat_child(&self, name: &OsStr) -> io::Result<Option<Observed>> {
        let name = name_cstring(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: the pinned parent fd is live, name is NUL-terminated, and
        // fstatat writes the supplied stat buffer on success.
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: fstatat initialized stat on success.
        regular_observation(&unsafe { stat.assume_init() }).map(Some)
    }

    pub(super) fn open_child(&self, name: &OsStr, access: OpenAccess) -> io::Result<File> {
        let name = name_cstring(name)?;
        let flags = match access {
            OpenAccess::ReadOnly => libc::O_RDONLY,
            OpenAccess::ReadWrite => libc::O_RDWR,
            OpenAccess::Create => libc::O_RDWR | libc::O_CREAT,
            OpenAccess::CreateNew => libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        };
        // SAFETY: the pinned parent fd is live; name is NUL-terminated; the
        // returned fd is immediately owned by a File below. O_NONBLOCK makes
        // a swapped FIFO refuse at fstat rather than hanging in openat.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was returned by the successful openat above.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub(super) fn delete_child(&self, name: &OsStr, sync_parent: bool) -> io::Result<()> {
        let name = name_cstring(name)?;
        // SAFETY: pinned parent fd is live and name is NUL-terminated. The
        // unlink can affect only this one leaf inside the pinned directory.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        if sync_parent {
            self.directory.sync_all()?;
        }
        Ok(())
    }
}

pub(super) fn observe_file(file: &File) -> io::Result<Observed> {
    regular_observation(&fstat(file.as_raw_fd())?)
}

pub(super) fn sqlite_header_mode(file: &File) -> io::Result<Option<(u8, u8)>> {
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(None);
    }
    if size < 20 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing SQLite main is shorter than its header",
        ));
    }
    let mut header = [0u8; 20];
    let mut read = 0;
    while read < header.len() {
        let count = file.read_at(&mut header[read..], read as u64)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "SQLite header changed during admission",
            ));
        }
        read += count;
    }
    if &header[..16] != b"SQLite format 3\0" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing code-map main is not a SQLite database",
        ));
    }
    Ok(Some((header[18], header[19])))
}

// SQLite's rollback locks occupy bytes above the largest signed 30-bit file
// offset. Use the same byte ranges as the stock Unix VFS so other processes
// and ordinary SQLite connections participate in the same protocol.
const PENDING_BYTE: libc::off_t = 0x4000_0000;
const RESERVED_BYTE: libc::off_t = PENDING_BYTE + 1;
const SHARED_FIRST: libc::off_t = PENDING_BYTE + 2;
const SHARED_SIZE: libc::off_t = 510;

#[derive(Default)]
struct InodeLocks {
    shared: HashSet<u64>,
    reserved: Option<u64>,
    pending: Option<u64>,
    exclusive: Option<u64>,
    deferred_close: Vec<File>,
}

fn lock_table() -> &'static Mutex<HashMap<Identity, InodeLocks>> {
    static TABLE: OnceLock<Mutex<HashMap<Identity, InodeLocks>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_lock(
    file: &File,
    kind: libc::c_short,
    start: libc::off_t,
    len: libc::off_t,
) -> io::Result<()> {
    let mut request = libc::flock {
        l_start: start,
        l_len: len,
        l_pid: 0,
        l_type: kind,
        l_whence: libc::SEEK_SET as libc::c_short,
    };
    // SAFETY: the descriptor is live and request points to a fully initialized
    // flock. F_SETLK is nonblocking; SQLite applies its own busy timeout.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &raw mut request) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn lock_result(error: &io::Error) -> libc::c_int {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => ffi::SQLITE_BUSY,
        _ => ffi::SQLITE_IOERR_LOCK,
    }
}

/// One SQLite main-file handle's state. POSIX record locks belong to the
/// process, not its file descriptor: dropping *any* descriptor for the inode
/// releases all of them. The table therefore counts same-process holders and
/// defers every close while any handle still owns a lock.
pub(super) struct RollbackLock {
    identity: Identity,
    id: u64,
    level: libc::c_int,
}

impl RollbackLock {
    pub(super) fn new(identity: Identity) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            identity,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            level: ffi::SQLITE_LOCK_NONE,
        }
    }

    pub(super) fn lock(&mut self, file: &File, wanted: libc::c_int) -> libc::c_int {
        if !(ffi::SQLITE_LOCK_SHARED..=ffi::SQLITE_LOCK_EXCLUSIVE).contains(&wanted) {
            return ffi::SQLITE_IOERR_LOCK;
        }
        if wanted <= self.level {
            return ffi::SQLITE_OK;
        }
        let mut table = lock_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = table.entry(self.identity).or_default();
        if self.level < ffi::SQLITE_LOCK_SHARED {
            if state.pending.is_some_and(|owner| owner != self.id)
                || state.exclusive.is_some_and(|owner| owner != self.id)
            {
                return ffi::SQLITE_BUSY;
            }
            if state.shared.is_empty() {
                if let Err(error) = file_lock(file, libc::F_RDLCK as _, PENDING_BYTE, 1) {
                    return lock_result(&error);
                }
                let shared = file_lock(file, libc::F_RDLCK as _, SHARED_FIRST, SHARED_SIZE);
                let pending_release = file_lock(file, libc::F_UNLCK as _, PENDING_BYTE, 1);
                if let Err(error) = shared {
                    return lock_result(&error);
                }
                if pending_release.is_err() {
                    return ffi::SQLITE_IOERR_LOCK;
                }
            }
            state.shared.insert(self.id);
            self.level = ffi::SQLITE_LOCK_SHARED;
        }
        if wanted >= ffi::SQLITE_LOCK_RESERVED && self.level < ffi::SQLITE_LOCK_RESERVED {
            if state.reserved.is_some_and(|owner| owner != self.id) {
                return ffi::SQLITE_BUSY;
            }
            if let Err(error) = file_lock(file, libc::F_WRLCK as _, RESERVED_BYTE, 1) {
                return lock_result(&error);
            }
            state.reserved = Some(self.id);
            self.level = ffi::SQLITE_LOCK_RESERVED;
        }
        if wanted >= ffi::SQLITE_LOCK_PENDING && self.level < ffi::SQLITE_LOCK_PENDING {
            if state.pending.is_some_and(|owner| owner != self.id) {
                return ffi::SQLITE_BUSY;
            }
            if let Err(error) = file_lock(file, libc::F_WRLCK as _, PENDING_BYTE, 1) {
                return lock_result(&error);
            }
            state.pending = Some(self.id);
            self.level = ffi::SQLITE_LOCK_PENDING;
        }
        if wanted == ffi::SQLITE_LOCK_EXCLUSIVE && self.level < ffi::SQLITE_LOCK_EXCLUSIVE {
            if state.shared.len() != 1 || !state.shared.contains(&self.id) {
                return ffi::SQLITE_BUSY;
            }
            if let Err(error) = file_lock(file, libc::F_WRLCK as _, SHARED_FIRST, SHARED_SIZE) {
                return lock_result(&error);
            }
            state.exclusive = Some(self.id);
            self.level = ffi::SQLITE_LOCK_EXCLUSIVE;
        }
        ffi::SQLITE_OK
    }

    pub(super) fn unlock(&mut self, file: &File, wanted: libc::c_int) -> libc::c_int {
        if wanted != ffi::SQLITE_LOCK_NONE && wanted != ffi::SQLITE_LOCK_SHARED {
            return ffi::SQLITE_IOERR_UNLOCK;
        }
        if wanted >= self.level {
            return ffi::SQLITE_OK;
        }
        let mut table = lock_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = table.get_mut(&self.identity) else {
            return ffi::SQLITE_IOERR_UNLOCK;
        };
        if self.level == ffi::SQLITE_LOCK_EXCLUSIVE {
            let kind = if wanted == ffi::SQLITE_LOCK_SHARED {
                libc::F_RDLCK
            } else {
                libc::F_UNLCK
            };
            if file_lock(file, kind as _, SHARED_FIRST, SHARED_SIZE).is_err() {
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            state.exclusive = None;
        }
        if state.pending == Some(self.id) {
            if file_lock(file, libc::F_UNLCK as _, PENDING_BYTE, 1).is_err() {
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            state.pending = None;
        }
        if state.reserved == Some(self.id) {
            if file_lock(file, libc::F_UNLCK as _, RESERVED_BYTE, 1).is_err() {
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            state.reserved = None;
        }
        if wanted == ffi::SQLITE_LOCK_NONE {
            if state.shared.len() == 1
                && file_lock(file, libc::F_UNLCK as _, SHARED_FIRST, SHARED_SIZE).is_err()
            {
                return ffi::SQLITE_IOERR_UNLOCK;
            }
            state.shared.remove(&self.id);
            self.level = ffi::SQLITE_LOCK_NONE;
            if state.shared.is_empty() {
                // Release parked descriptors while holding the table mutex,
                // before any other local handle can acquire a new POSIX lock.
                table.remove(&self.identity);
            }
        } else {
            self.level = ffi::SQLITE_LOCK_SHARED;
        }
        ffi::SQLITE_OK
    }

    pub(super) fn check_reserved(&self, file: &File, result: &mut libc::c_int) -> libc::c_int {
        let table = lock_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if table
            .get(&self.identity)
            .is_some_and(|state| state.reserved.is_some())
        {
            *result = 1;
            return ffi::SQLITE_OK;
        }
        let mut request = libc::flock {
            l_start: RESERVED_BYTE,
            l_len: 1,
            l_pid: 0,
            l_type: libc::F_WRLCK as _,
            l_whence: libc::SEEK_SET as _,
        };
        // SAFETY: file and output structure remain live for F_GETLK.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &raw mut request) } == -1 {
            return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
        }
        *result = i32::from(request.l_type != libc::F_UNLCK as libc::c_short);
        ffi::SQLITE_OK
    }

    pub(super) fn close(mut self, file: File) -> libc::c_int {
        let result = if self.level > ffi::SQLITE_LOCK_NONE {
            self.unlock(&file, ffi::SQLITE_LOCK_NONE)
        } else {
            ffi::SQLITE_OK
        };
        let mut table = lock_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if result != ffi::SQLITE_OK {
            // An uncertain unlock must not close this descriptor and discard
            // another SQLite connection's process-wide lock as a side effect.
            table
                .entry(self.identity)
                .or_default()
                .deferred_close
                .push(file);
            return result;
        }
        if let Some(state) = table.get_mut(&self.identity) {
            if !state.shared.is_empty() {
                state.deferred_close.push(file);
                return ffi::SQLITE_OK;
            }
            table.remove(&self.identity);
        }
        drop(file);
        ffi::SQLITE_OK
    }
}

pub(super) fn close_unlocked(file: File, identity: Identity) {
    let mut table = lock_table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = table.get_mut(&identity) {
        if !state.shared.is_empty() {
            state.deferred_close.push(file);
            return;
        }
        table.remove(&identity);
    }
    drop(file);
}
