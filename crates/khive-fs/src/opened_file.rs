//! The path behind an open file, a contained open, and a Unix final-component no-follow open.
//!
//! A path checked before it is opened can be swapped for a symlink in between. The helpers here
//! judge the file that was actually opened: [`opened_file_path`] asks the kernel which path an
//! open handle refers to, and [`open_regular_file_within`] requires that resolved path to lie
//! inside a canonical root.
//!
//! Errors carry the raw `io::Error` of the failing call and attach no context; callers map each
//! [`ContainedOpenError`] variant into their own error type and message.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Why a regular-file open refused a path.
#[derive(Debug)]
pub enum ContainedOpenError {
    /// The open itself failed.
    Open(io::Error),
    /// The type of the opened file could not be read.
    Metadata(io::Error),
    /// The opened file is not a regular file.
    NotRegular,
    /// The path behind the opened file could not be resolved.
    Resolve(io::Error),
    /// The resolved path of the opened file is not inside the root.
    Escapes {
        /// The resolved path of the opened file.
        opened: PathBuf,
    },
}

/// Open a regular file read-only without following a final-component symlink on Unix.
///
/// Earlier symlinks resolve normally; this does not enforce containment or ancestor trust.
/// Trailing separators or `/.` can make the preceding component an ancestor. Non-blocking
/// open prevents a FIFO waiting for a writer before the opened-file type check refuses it.
/// The returned file is close-on-exec. Open and metadata failures retain their raw errors.
#[cfg(unix)]
pub fn open_regular_file_nofollow(path: &Path) -> Result<File, ContainedOpenError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(ContainedOpenError::Open)?;
    let metadata = file.metadata().map_err(ContainedOpenError::Metadata)?;
    if !metadata.is_file() {
        return Err(ContainedOpenError::NotRegular);
    }
    Ok(file)
}

/// Open `path` read-only and return the file only if it is a regular file inside `canonical_root`.
///
/// The check reads the opened handle, not the pathname, so a source that was swapped for a
/// symlink after an earlier check is judged by where it really lands. `canonical_root` must
/// already be canonical: it is compared as a path prefix with the resolved path of the opened
/// file. On Unix the open is non-blocking and close-on-exec, so a FIFO named by `path` cannot
/// stall the open before the regular-file check refuses it.
pub fn open_regular_file_within(
    canonical_root: &Path,
    path: &Path,
) -> Result<File, ContainedOpenError> {
    #[cfg(unix)]
    let result = {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt as _;

        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
    };
    #[cfg(not(unix))]
    let result = File::open(path);
    let file = result.map_err(ContainedOpenError::Open)?;
    let metadata = file.metadata().map_err(ContainedOpenError::Metadata)?;
    if !metadata.is_file() {
        return Err(ContainedOpenError::NotRegular);
    }
    let opened = opened_file_path(&file).map_err(ContainedOpenError::Resolve)?;
    if !opened.starts_with(canonical_root) {
        return Err(ContainedOpenError::Escapes { opened });
    }
    Ok(file)
}

/// The path the kernel reports for the file behind `file`, read from `/proc/self/fd`.
///
/// The path is resolved, so it names the file that was opened and not a symlink that led to it.
/// Removed files and directories keep the kernel's `" (deleted)"` suffix.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;

    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// The path the kernel reports for the file behind `file`, read with `F_GETPATH`.
///
/// The path is resolved, so it names the file that was opened and not a symlink that led to it.
/// This query is available on every Apple target.
#[cfg(target_vendor = "apple")]
pub fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let mut bytes = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `file` owns a live descriptor; fcntl writes into this buffer
    // during the call and does not retain its pointer.
    let result = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETPATH,
            bytes.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let length = bytes.iter().position(|byte| *byte == 0).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "F_GETPATH returned no NUL terminator",
        )
    })?;
    Ok(PathBuf::from(OsStr::from_bytes(&bytes[..length])))
}

/// The path the kernel reports for the file behind `file`, read from the final path name of the
/// handle.
///
/// The path is resolved, so it names the file that was opened and not a link that led to it.
#[cfg(windows)]
pub fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };

    let mut path = vec![0_u16; 260];
    loop {
        // SAFETY: `file` owns a live handle and the buffer is writable for
        // the duration of this call.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                path.as_mut_ptr(),
                path.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < path.len() {
            path.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&path)));
        }
        path.resize(length.saturating_add(1), 0);
    }
}

/// Always `ErrorKind::Unsupported`: this Unix target has no query for the path behind a
/// descriptor.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
pub fn opened_file_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure opened-file path resolution is unsupported on this Unix target",
    ))
}

/// Always `ErrorKind::Unsupported`: this target has no query for the path behind an open file.
#[cfg(not(any(unix, windows)))]
pub fn opened_file_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure opened-file path resolution is unsupported on this target",
    ))
}
