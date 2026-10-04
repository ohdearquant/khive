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
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

/// Convert one path component into a NUL-terminated C string.
///
/// A name with an interior NUL byte is refused with `ErrorKind::InvalidInput`, because the
/// kernel would otherwise see a shorter name than the caller meant.
pub fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
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
