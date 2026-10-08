//! Descriptor-relative filesystem helpers for Unix.
//!
//! Each helper resolves one path component against an already open directory
//! descriptor instead of walking a pathname from the filesystem root, so a caller that holds
//! a verified directory handle is not steered elsewhere by a symlink swapped in afterwards.
//! The helpers return the raw `io::Error` of the failing call and attach no context;
//! callers map the error into their own types.
//!
//! The thread `errno` accessors live here as well. `readdir` reports a read error only
//! through `errno`, and every platform spells the accessor differently.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

/// Convert one path component into a NUL-terminated C string.
///
/// Every helper in this module hands the name to an `*at` system call, which reads it as a path.
/// A name that is not one component is therefore refused with `ErrorKind::InvalidInput` before
/// any system call: an empty name and a name containing `/`. A name starting with `/` would
/// ignore the parent descriptor, and in `a/b` the no-follow flags apply to `b` only.
///
/// `.` and `..` are single components and are accepted: `list_names` reopens a directory through
/// `.`, and a directory walk steps up through `..`. What `..` means is the caller's policy.
///
/// A name with an interior NUL byte is refused the same way, because the kernel would otherwise
/// see a shorter name than the caller meant.
pub fn c_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path component must be a single name",
        ));
    }
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path component"))
}

/// `fstat` on an open file.
pub fn stat_fd(file: &File) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::uninit();
    // SAFETY: `file` owns a live descriptor and `stat` is a writable out-parameter.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `fstat` initialized the entire structure.
    Ok(unsafe { stat.assume_init() })
}

/// `fstatat` of `name` relative to `parent`, without following a symlink at `name`.
///
/// A symlink is reported as a symlink, never as the file it points to.
pub fn stat_at(parent: &File, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::uninit();
    // SAFETY: `parent` is a live descriptor, `name` is NUL-terminated for the call, and `stat`
    // is a writable out-parameter.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `fstatat` initialized the entire structure.
    Ok(unsafe { stat.assume_init() })
}

/// Device and inode identity in the unsigned representation used by Unix `MetadataExt`.
///
/// Equality compares the object observed by each call, not its contents. It does not
/// prevent later replacement, inode reuse, or changes to the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    /// Read identity from the open handle using the standard library's metadata path.
    pub fn of(file: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    /// Read identity relative to `parent`, identifying a final symlink itself.
    ///
    /// Name validation and errors are those of [`stat_at`], including its native
    /// `fstatat` representation limits. No path containment policy is added.
    #[allow(clippy::unnecessary_cast)] // The native field widths and signedness vary by Unix target.
    pub fn at(parent: &File, name: &OsStr) -> io::Result<Self> {
        let stat = stat_at(parent, name)?;
        Ok(Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
        })
    }
}

/// Open `name` read-only relative to `parent`, refusing a symlink at the final component.
///
/// The open is close-on-exec and non-blocking. With `directory` set it also requires the
/// entry to be a directory.
pub fn open_at(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
    let name = c_name(name)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: `parent` is a live descriptor and `name` is NUL-terminated for the call.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by a successful `openat` and is owned here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Creation policy for a descriptor-relative writable open.
#[derive(Debug, Clone, Copy)]
pub enum Create {
    /// Open an existing entry; do not create one.
    No,
    /// Create the entry if it is missing, preserving existing contents otherwise.
    IfMissing,
    /// Create a new entry, refusing any existing entry.
    Exclusive,
}

/// Options for [`open_file_at`]. Opens never truncate existing contents.
#[derive(Debug, Clone, Copy)]
pub struct OpenFileOptions {
    /// Open read-write when true, or write-only when false.
    pub read_write: bool,
    pub create: Create,
    /// Add `O_NONBLOCK` to the open.
    pub nonblock: bool,
    /// Creation permissions, filtered by the process umask; ignored for existing entries.
    pub mode: u32,
}

/// Open one writable entry relative to a borrowed directory descriptor.
///
/// Always uses `O_NOFOLLOW | O_CLOEXEC`. The name passes through [`c_name`], with
/// an additional refusal of `.` and `..`; the older read/directory helpers keep
/// their existing dot-component policy. This does not require a regular file.
pub fn open_file_at(parent: BorrowedFd<'_>, name: &str, opts: OpenFileOptions) -> io::Result<File> {
    let name = c_name(OsStr::new(name))?;
    if matches!(name.to_bytes(), b"." | b"..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dot components are not writable names",
        ));
    }
    let access = if opts.read_write {
        libc::O_RDWR
    } else {
        libc::O_WRONLY
    };
    let create = match opts.create {
        Create::No => 0,
        Create::IfMissing => libc::O_CREAT,
        Create::Exclusive => libc::O_CREAT | libc::O_EXCL,
    };
    let flags = access
        | create
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if opts.nonblock { libc::O_NONBLOCK } else { 0 };
    // SAFETY: parent is borrowed and live, the validated name is NUL-terminated,
    // and mode is passed as the promoted unsigned integer required by openat's varargs.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, opts.mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful openat returned a new descriptor owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Open `name` as a directory relative to `parent`, refusing a symlink at the final component.
///
/// The returned handle is close-on-exec and bound to that exact inode.
pub fn open_dir_at(parent: &File, name: &OsStr) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: `parent` is a live descriptor and `name` is NUL-terminated for the call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by a successful `openat` and is owned here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Pointer to the calling thread's `errno` cell.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "freebsd"
))]
pub fn errno_location() -> *mut libc::c_int {
    // SAFETY: the accessor returns the live `errno` cell of the calling thread.
    unsafe { libc::__error() }
}

/// Pointer to the calling thread's `errno` cell.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "emscripten",
    target_os = "redox",
    target_os = "hurd"
))]
pub fn errno_location() -> *mut libc::c_int {
    // SAFETY: the accessor returns the live `errno` cell of the calling thread.
    unsafe { libc::__errno_location() }
}

/// Pointer to the calling thread's `errno` cell.
#[cfg(any(
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
    target_os = "cygwin",
    target_os = "nuttx"
))]
pub fn errno_location() -> *mut libc::c_int {
    // SAFETY: the accessor returns the live `errno` cell of the calling thread.
    unsafe { libc::__errno() }
}

/// Pointer to the calling thread's `errno` cell.
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
pub fn errno_location() -> *mut libc::c_int {
    // SAFETY: the accessor returns the live `errno` cell of the calling thread.
    unsafe { libc::___errno() }
}

/// Pointer to the calling thread's `errno` cell.
#[cfg(target_os = "aix")]
pub fn errno_location() -> *mut libc::c_int {
    // SAFETY: the accessor returns the live `errno` cell of the calling thread.
    unsafe { libc::_Errno() }
}

/// Pointer to the calling thread's `errno` cell.
#[cfg(target_os = "haiku")]
pub fn errno_location() -> *mut libc::c_int {
    // SAFETY: the accessor returns the live `errno` cell of the calling thread.
    unsafe { libc::_errnop() }
}

/// Zero `errno` on the current thread.
///
/// `readdir` never clears `errno` itself on success, so this must run immediately before
/// each call for the NULL-return ambiguity to be resolvable afterward.
pub fn clear_errno() {
    // SAFETY: `errno_location` returns a valid, live thread-local `c_int` cell for the
    // duration of this call.
    unsafe { *errno_location() = 0 };
}

/// The current thread's `errno`.
pub fn current_errno() -> libc::c_int {
    // SAFETY: see `clear_errno`.
    unsafe { *errno_location() }
}

struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the successful `fdopendir` result.
        unsafe { libc::closedir(self.0) };
    }
}

/// List the entry names of `directory`, skipping `.` and `..`, sorted by byte order.
///
/// The directory is reopened through `.` so the listing has its own position and leaves the
/// position of the caller's descriptor alone; a duplicate would share it. `errno` is cleared
/// before every `readdir`, so a NULL return with a nonzero `errno` is reported as an error
/// instead of ending the listing early.
pub fn list_names(directory: &File) -> io::Result<Vec<OsString>> {
    let fd = open_at(directory, OsStr::new("."), true)?.into_raw_fd();
    // SAFETY: `fd` is uniquely owned and `fdopendir` takes ownership of it on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so ownership of `fd` remains here.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let stream = DirStream(stream);
    let mut result = Vec::new();
    loop {
        clear_errno();
        // SAFETY: `stream` holds a live `DIR*` until this function returns.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error);
            }
            break;
        }
        // SAFETY: `d_name` is NUL-terminated and is copied out before the next `readdir`.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            result.push(OsString::from_vec(name.to_vec()));
        }
    }
    result.sort();
    Ok(result)
}
