#[cfg(not(any(unix, windows)))]
use std::fs;
use std::path::{Path, PathBuf};

use khive_storage::blob::ContentRef;
#[cfg(unix)]
use khive_storage::types::StorageResult;

#[cfg(unix)]
use super::{map_io_err, ROOT_WRITE_LOCK_FILE};
use crate::error::SqliteError;

#[cfg(unix)]
pub(super) fn open_blob_root_handle(root: &Path) -> std::io::Result<std::fs::File> {
    open_dir_no_follow(root)
}

#[cfg(windows)]
pub(super) fn open_blob_root_handle(root: &Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    let handle = OpenOptions::new()
        .read(true)
        // Retain the handle for the store's lifetime. Omitting
        // FILE_SHARE_DELETE prevents the root's final component from being
        // renamed or removed while this store can still issue operations.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root)?;
    let file_type = handle.metadata()?.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "blob store root is not a directory or is a reparse point: {}",
                root.display()
            ),
        ));
    }
    Ok(handle)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_blob_root_handle(root: &Path) -> std::io::Result<std::fs::File> {
    let handle = std::fs::File::open(root)?;
    if !handle.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("blob store root is not a directory: {}", root.display()),
        ));
    }
    Ok(handle)
}

/// Fail closed when the canonical root spelling no longer names the same
/// directory handle retained at construction. The comparison is only an
/// invariant check: filesystem authority for the operation itself remains
/// the retained handle, so a replacement after this check cannot redirect a
/// handle-relative walk.
#[cfg(unix)]
pub(super) fn verify_blob_root_identity(
    root: &Path,
    root_handle: &std::fs::File,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let current = open_dir_no_follow(root).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "blob store root is no longer reachable as its initialization-time directory ({}): {error}",
                root.display()
            ),
        )
    })?;
    let expected = root_handle.metadata()?;
    let current = current.metadata()?;
    if expected.dev() != current.dev() || expected.ino() != current.ino() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "blob store root no longer names its initialization-time directory: {}",
                root.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn verify_blob_root_identity(
    root: &Path,
    _root_handle: &std::fs::File,
) -> std::io::Result<()> {
    // `root` is canonicalized before the retained handle opens. Re-resolving
    // the spelling catches an ancestor redirect on Windows; the retained
    // no-share-delete handle separately pins the root's final component, and
    // the Windows leaf helpers compare their resolved target handles to that
    // initialization-time root handle before use.
    let current = root.canonicalize().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "blob store root is no longer reachable as its initialization-time directory ({}): {error}",
                root.display()
            ),
        )
    })?;
    if current != root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "blob store root no longer names its initialization-time directory: {}",
                root.display()
            ),
        ));
    }
    Ok(())
}

/// Unlink one blob's shard-relative file using `O_NOFOLLOW`-verified
/// descriptor traversal instead of a plain path-based delete.
///
/// A path-based `fs::remove_file(shard_path(root, content_ref))` resolves
/// every path component through the kernel exactly like any other path
/// lookup. If either shard-directory level (`root/<hex[0..2]>` or
/// `root/<hex[0..2]>/<hex[2..4]>`) has been replaced with a symlink —
/// through a misconfigured root, a shared/writable parent directory, or a
/// race with another process — that lookup follows it and can unlink a file
/// entirely outside the blob root. Each shard component is instead opened
/// relative to the previous, already-verified descriptor with
/// `O_DIRECTORY | O_NOFOLLOW`, so a symlink planted at either level is
/// refused (`ELOOP`) rather than followed, and the final `unlinkat` runs
/// relative to the verified leaf descriptor rather than a re-resolved path
/// string. Same fd-pinned idiom as `khive-db`'s walpin sidecar writes
/// (`walpin.rs`) and `khive-vamana`'s external-id sidecar
/// (`external_ids.rs`) use for the same TOCTOU hazard.
#[cfg(unix)]
pub(super) fn unlink_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    verify_blob_root_identity(root, root_handle)?;
    let hex = content_ref.as_str();
    let shard1_dir = openat_dir_no_follow(root_handle.as_raw_fd(), &hex[0..2])?;
    let shard2_dir = openat_dir_no_follow(shard1_dir.as_raw_fd(), &hex[2..4])?;
    // `unlinkat` removes this directory entry itself rather than following a
    // leaf symlink, matching the original delete semantics.
    unlink_entry_at(shard2_dir.as_raw_fd(), hex)
}

#[cfg(windows)]
pub(super) fn unlink_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
) -> std::io::Result<()> {
    // Windows equivalent of the Unix arm's fd-pinned walk. The Unix arm's
    // guarantee is: the root's ancestors are resolved exactly once (at
    // `open(root)`), every later step is relative to an already-verified
    // open descriptor, and the final unlink acts on a descriptor, never on
    // a re-resolved path string. This arm reproduces each property from
    // handle semantics:
    //
    // 1. No-follow verification BY HANDLE: each directory level is opened
    //    with `FILE_FLAG_OPEN_REPARSE_POINT` (plus `FILE_FLAG_BACKUP_SEMANTICS`,
    //    which is what permits opening a directory handle at all), so a
    //    junction or symlink planted at that level yields a handle to the
    //    reparse point itself rather than to its target, and the
    //    handle-derived metadata (`File::metadata`, which queries the handle,
    //    not a re-resolved path) exposes it for refusal.
    // 2. Pinning: the directory handles are opened WITHOUT
    //    `FILE_SHARE_DELETE`. Deleting or renaming a directory requires an
    //    open with `DELETE` access, which fails with a sharing violation
    //    while these handles are held, so no checked component can be
    //    swapped for the duration of the call.
    // 3. Deletion BY HANDLE with a handle-anchored identity check. A
    //    path-based `remove_file` here would re-resolve the full path from
    //    the volume root, so a reparse point swapped at an UNPINNED ancestor
    //    of `root` (which this function cannot pin — it does not own them)
    //    could redirect the delete outside the blob root even while all
    //    three pins hold. Instead the target file itself is opened with
    //    `DELETE` access and `FILE_FLAG_OPEN_REPARSE_POINT` (a symlink leaf
    //    opens as the link entry, matching `unlinkat` semantics), its TRUE
    //    resolved path is read back from the handle with
    //    `GetFinalPathNameByHandleW`, and the delete proceeds only if that
    //    path equals the root pin's own handle-final path extended by the
    //    verified shard components. The root pin's final path is a property
    //    of the already-open handle — an ancestor swapped after the pin
    //    opened cannot change it, while it does change (and thereby betrays)
    //    the file handle's resolution. The delete itself is
    //    `SetFileInformationByHandle(FileDispositionInfo)` on the verified
    //    handle: no path is ever re-resolved between check and use.
    //
    // An ancestor reparse point already in place BEFORE the root pin opens
    // resolves identically for the pin and the file and is accepted — the
    // same exposure the Unix arm accepts for a symlinked ancestor at
    // `open(root)` time; that is the operator's configured deployment, not
    // a check-to-use window.
    use std::fs::OpenOptions;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, GetFinalPathNameByHandleW, SetFileInformationByHandle,
        FILE_DISPOSITION_INFO,
    };

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// `FILE_NAME_NORMALIZED | VOLUME_NAME_DOS` — both zero; named for the
    /// contract (normalized on-disk case, drive-letter form) rather than
    /// passing a bare 0.
    const FINAL_PATH_FLAGS: u32 = 0x0;

    fn open_dir_pinned_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
        let dir = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let file_type = dir.metadata()?.file_type();
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to unlink blob shard file through non-directory or \
                     reparse-point path component: {}",
                    path.display()
                ),
            ));
        }
        Ok(dir)
    }

    /// The handle's true, fully resolved path (`GetFinalPathNameByHandleW`,
    /// normalized `\\?\`-prefixed DOS form). A handle property: later
    /// changes to any directory the original path traversed cannot alter it.
    fn final_path_by_handle(file: &std::fs::File) -> std::io::Result<std::path::PathBuf> {
        let handle = file.as_raw_handle();
        let mut buf: Vec<u16> = vec![0; 512];
        loop {
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    handle as _,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    FINAL_PATH_FLAGS,
                )
            };
            if len == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let len = len as usize;
            if len <= buf.len() {
                buf.truncate(len);
                return Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(
                    &buf,
                )));
            }
            // Returned length is the required buffer size (in wide chars,
            // including the terminator) when the buffer was too small.
            buf.resize(len, 0);
        }
    }

    verify_blob_root_identity(root, root_handle)?;
    let hex = content_ref.as_str();
    let shard1 = root.join(&hex[0..2]);
    let shard2 = shard1.join(&hex[2..4]);
    let _shard1_pin = open_dir_pinned_no_follow(&shard1)?;
    let _shard2_pin = open_dir_pinned_no_follow(&shard2)?;

    let expected = final_path_by_handle(root_handle)?
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(hex);

    // Sharing READ|WRITE but NOT DELETE: renaming or deleting a file requires
    // an open with `DELETE` access, which fails with a sharing violation while
    // this handle is held, so the file whose final path is validated below is
    // the same file the disposition call deletes — the leaf is pinned exactly
    // like the directory components above.
    let target = OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(shard2.join(hex))?;

    let resolved = final_path_by_handle(&target)?;
    if resolved != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing blob delete: handle resolved outside the verified blob root \
                 (expected {}, resolved {})",
                expected.display(),
                resolved.display()
            ),
        ));
    }

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let ok = unsafe {
        SetFileInformationByHandle(
            target.as_raw_handle() as _,
            FileDispositionInfo,
            std::ptr::from_ref(&disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(super) fn unlink_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
) -> std::io::Result<()> {
    // Neither `openat`/`O_NOFOLLOW` nor Windows directory-handle pinning is
    // available on this tier (which no release artifact targets), so this
    // checks each shard path component with `symlink_metadata` before the
    // delete. `std::fs::symlink_metadata` reports symlinks through
    // `file_type().is_symlink()` without following them, so a link planted
    // at the root or either shard level is refused rather than walked into
    // by the final `remove_file`.
    //
    // Residual limitation, accepted for this descriptorless platform tier:
    // nothing here holds an open, referentially-verified handle on the
    // checked directories between this check and the `remove_file` call
    // below, so a component could still be swapped for a link in that
    // window (TOCTOU).
    verify_blob_root_identity(root, root_handle)?;
    let hex = content_ref.as_str();
    let shard1 = root.join(&hex[0..2]);
    let shard2 = shard1.join(&hex[2..4]);
    for component in [root, shard1.as_path(), shard2.as_path()] {
        let metadata = fs::symlink_metadata(component)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to unlink blob shard file through symlinked path component: {}",
                    component.display()
                ),
            ));
        }
    }
    fs::remove_file(shard2.join(hex))
}

/// Open one blob leaf without following any root/shard/leaf symlink and
/// return the single handle that must remain the authority for metadata,
/// bounded bytes, and digest verification.
#[cfg(unix)]
pub(super) fn open_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
) -> std::io::Result<std::fs::File> {
    verify_blob_root_identity(root, root_handle)?;
    open_blob_shard_file_at_no_follow(root_handle, content_ref, libc::O_RDONLY)
}

#[cfg(windows)]
pub(super) fn open_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
) -> std::io::Result<std::fs::File> {
    open_blob_shard_file_no_follow_windows(root, root_handle, content_ref, false)
}

#[cfg(windows)]
pub(super) fn open_blob_shard_file_no_follow_windows(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
    for_mtime_update: bool,
) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FINAL_PATH_FLAGS: u32 = 0x0;

    fn open_dir_pinned_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
        let dir = OpenOptions::new()
            .read(true)
            // Omitting FILE_SHARE_DELETE pins this checked component against
            // rename/removal until the leaf is open and verified.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let file_type = dir.metadata()?.file_type();
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to read a blob through non-directory or reparse-point component: {}",
                    path.display()
                ),
            ));
        }
        Ok(dir)
    }

    fn final_path_by_handle(file: &std::fs::File) -> std::io::Result<PathBuf> {
        let mut buf: Vec<u16> = vec![0; 512];
        loop {
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle() as _,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    FINAL_PATH_FLAGS,
                )
            };
            if len == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let len = len as usize;
            if len <= buf.len() {
                buf.truncate(len);
                return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
            }
            buf.resize(len, 0);
        }
    }

    verify_blob_root_identity(root, root_handle)?;
    let hex = content_ref.as_str();
    let shard1 = root.join(&hex[0..2]);
    let shard2 = shard1.join(&hex[2..4]);
    let _shard1_pin = open_dir_pinned_no_follow(&shard1)?;
    let _shard2_pin = open_dir_pinned_no_follow(&shard2)?;
    let expected = final_path_by_handle(root_handle)?
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(hex);

    let target = OpenOptions::new()
        .read(true)
        .write(for_mtime_update)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(shard2.join(hex))?;
    let file_type = target.metadata()?.file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to read a blob leaf that is not a regular file",
        ));
    }
    let resolved = final_path_by_handle(&target)?;
    if resolved != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing blob read: handle resolved outside the verified blob root (expected {}, resolved {})",
                expected.display(),
                resolved.display()
            ),
        ));
    }
    Ok(target)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    _content_ref: &ContentRef,
) -> std::io::Result<std::fs::File> {
    verify_blob_root_identity(root, root_handle)?;
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "bounded verified blob reads require handle-relative no-follow file APIs",
    ))
}

#[cfg(unix)]
fn open_dir_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c_path` is NUL-terminated for the call; a successful fd is
    // uniquely owned and wrapped immediately below.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by the successful `open` above and is
    // uniquely owned by this `File`, which closes it exactly once on drop.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
pub(super) fn openat_dir_no_follow(
    parent_fd: std::os::unix::io::RawFd,
    name: &str,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;

    let c_name = std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c_name` is NUL-terminated; `parent_fd` is a live, open
    // directory descriptor for the duration of this call. A successful fd
    // is uniquely owned and wrapped immediately below.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by the successful `openat` above and is
    // uniquely owned by this `File`, which closes it exactly once on drop.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
pub(super) fn openat_regular_file_no_follow(
    parent_fd: std::os::unix::io::RawFd,
    name: &str,
    access_flags: libc::c_int,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;

    let c_name = std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // O_NONBLOCK prevents a planted FIFO/device-like entry from hanging the
    // worker before handle metadata can reject it. It is inert for regular
    // files.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            c_name.as_ptr(),
            access_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was newly returned and is transferred exactly once.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing non-regular blob store entry: {name}"),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_blob_shard_file_at_no_follow(
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
    access_flags: libc::c_int,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;

    let hex = content_ref.as_str();
    let shard1_dir = openat_dir_no_follow(root_handle.as_raw_fd(), &hex[0..2])?;
    let shard2_dir = openat_dir_no_follow(shard1_dir.as_raw_fd(), &hex[2..4])?;
    openat_regular_file_no_follow(shard2_dir.as_raw_fd(), hex, access_flags)
}

#[cfg(unix)]
pub(super) fn open_or_create_dir_at_no_follow(
    parent_fd: std::os::unix::io::RawFd,
    name: &str,
) -> std::io::Result<std::fs::File> {
    let c_name = std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    match openat_dir_no_follow(parent_fd, name) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // SAFETY: `parent_fd` is live and `c_name` is NUL-terminated.
            let rc = unsafe { libc::mkdirat(parent_fd, c_name.as_ptr(), 0o777) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            // Always validate the resulting entry on a descriptor. Another
            // process may have won the creation race with a non-directory or
            // a symlink.
            openat_dir_no_follow(parent_fd, name)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
pub(super) fn create_regular_file_at_no_follow(
    parent_fd: std::os::unix::io::RawFd,
    name: &str,
    mode: libc::mode_t,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;

    let c_name = std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `parent_fd` is live, `c_name` is NUL-terminated, and the mode
    // argument is supplied because O_CREAT is present.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            c_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was newly returned and is transferred exactly once.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
pub(super) fn unlink_entry_at(
    parent_fd: std::os::unix::io::RawFd,
    name: &str,
) -> std::io::Result<()> {
    let c_name = std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `parent_fd` is live and `c_name` is NUL-terminated.
    let rc = unsafe { libc::unlinkat(parent_fd, c_name.as_ptr(), 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn rename_entry_at(
    source_fd: std::os::unix::io::RawFd,
    from: &str,
    destination_fd: std::os::unix::io::RawFd,
    to: &str,
) -> std::io::Result<()> {
    let c_from = std::ffi::CString::new(from)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let c_to = std::ffi::CString::new(to)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both names are NUL-terminated and relative to live directory
    // handles retained from the same blob root.
    let rc = unsafe { libc::renameat(source_fd, c_from.as_ptr(), destination_fd, c_to.as_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn available_space_at(root_handle: &std::fs::File) -> std::io::Result<u64> {
    use std::os::unix::io::AsRawFd;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `root_handle` is live and `stat` is a valid output buffer.
    let rc = unsafe { libc::fstatvfs(root_handle.as_raw_fd(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // `f_bavail`'s width is platform-dependent (u64 on Linux glibc, u32 on
    // macOS), so the widening is a no-op on some targets and required on
    // others; the lint only sees one target at a time.
    #[allow(clippy::useless_conversion)]
    Ok(stat.f_frsize.saturating_mul(u64::from(stat.f_bavail)))
}

#[cfg(unix)]
pub(super) fn acquire_root_write_lock_at(
    root_handle: &std::fs::File,
) -> StorageResult<std::fs::File> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let c_name = std::ffi::CString::new(ROOT_WRITE_LOCK_FILE)
        .expect("the static blob root lock name contains no NUL");
    // SAFETY: the root handle is live, the static name is NUL-terminated,
    // and the mode is present because O_CREAT is used.
    let fd = unsafe {
        libc::openat(
            root_handle.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666,
        )
    };
    if fd < 0 {
        return Err(map_io_err(
            std::io::Error::last_os_error(),
            "root_write_lock_open",
        ));
    }
    // SAFETY: `fd` was newly returned and is transferred exactly once.
    let lock_file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !lock_file
        .metadata()
        .map_err(|e| map_io_err(e, "root_write_lock_metadata"))?
        .file_type()
        .is_file()
    {
        return Err(map_io_err(
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "blob root write lock is not a regular file",
            ),
            "root_write_lock_open",
        ));
    }
    fs4::FileExt::lock(&lock_file).map_err(|e| map_io_err(e, "root_write_lock_acquire"))?;
    Ok(lock_file)
}

/// Collision-resistant diagnostic identity for one canonical blob root.
///
/// Paths are not required to be UTF-8. Hash the platform-native path bytes
/// rather than a lossy display spelling. Recovery does not depend on this
/// mutable identity: exclusive database-scoped sweep ownership makes every
/// pre-existing claim abandoned before a new sweep starts, including claims
/// copied by backup or left under an earlier root spelling.
pub(super) fn blob_root_key(root: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        root.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = {
        use std::os::windows::ffi::OsStrExt;
        root.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    #[cfg(not(any(unix, windows)))]
    let bytes = root.to_string_lossy().as_bytes().to_vec();
    blake3::hash(&bytes).to_hex().to_string()
}

/// Resolve the blob store root directory.
///
/// Precedence (khive#292, SPEC-gate ruling): `KHIVE_BLOB_ROOT` env var >
/// caller-supplied `config_root` (resolved from `khive.toml` by a layer above
/// this crate — `khive-db` cannot parse TOML itself without an upward
/// dependency) > beside the database directory (`<db_dir>/blobs`). Errors
/// when none apply — an in-memory backend with no override and no env var has
/// no directory to default beside.
pub fn resolve_blob_root(
    db_dir: Option<&Path>,
    config_root: Option<&Path>,
) -> Result<PathBuf, SqliteError> {
    if let Ok(env_root) = std::env::var("KHIVE_BLOB_ROOT") {
        if !env_root.trim().is_empty() {
            return Ok(PathBuf::from(env_root));
        }
    }
    if let Some(root) = config_root {
        return Ok(root.to_path_buf());
    }
    if let Some(dir) = db_dir {
        return Ok(dir.join("blobs"));
    }
    Err(SqliteError::InvalidData(
        "cannot resolve a blob store root: no KHIVE_BLOB_ROOT env var, no configured \
         root, and the database has no on-disk directory to default beside (in-memory \
         backend)"
            .to_string(),
    ))
}
