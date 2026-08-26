//! Filesystem-backed `BlobStore` — content-addressed, BLAKE3-sharded on disk.
//!
//! Layout: `<root>/<hex[0..2]>/<hex[2..4]>/<hex>`, plus a root-local advisory
//! lock file. The two-level shard is identical in shape to git's loose-object
//! store, so a root holding millions of blobs never puts more than a few
//! thousand entries in one directory. Writes are atomic-publish (khive#292):
//! bytes land in a temporary entry in the SAME shard directory as the final
//! path (guaranteeing same-filesystem rename), the written length is checked
//! against the input length, then an atomic rename publishes the entry —
//! crash-safe (a crash mid-write leaves an orphaned temp file, never a
//! partially-committed blob).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use khive_storage::blob::{
    BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef, MAX_BLOB_WHOLE_BYTES,
};
use khive_storage::error::StorageError;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue, StorageResult};
use khive_storage::{AtomicUnitOp, SqlAccess, StorageCapability};

use crate::error::SqliteError;
use uuid::Uuid;

const ROOT_WRITE_LOCK_FILE: &str = ".khive-blob-write.lock";
const DATABASE_GC_LOCK_SUFFIX: &str = ".khive-blob-gc.lock";
/// Maximum candidates represented by one claim transaction and its matching
/// physical-delete/cleanup cycle. This bounds JSON binding, returned rows,
/// claim-table pages dirtied per transaction, and exclusive-writer hold time.
const BLOB_GC_CLAIM_BATCH_SIZE: usize = 128;

fn map_io_err(e: std::io::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Blob, op, e)
}

#[cfg(any(test, not(unix)))]
fn shard_path(root: &Path, content_ref: &ContentRef) -> PathBuf {
    let hex = content_ref.as_str();
    root.join(&hex[0..2]).join(&hex[2..4]).join(hex)
}

#[cfg(unix)]
fn open_blob_root_handle(root: &Path) -> std::io::Result<std::fs::File> {
    open_dir_no_follow(root)
}

#[cfg(windows)]
fn open_blob_root_handle(root: &Path) -> std::io::Result<std::fs::File> {
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
fn open_blob_root_handle(root: &Path) -> std::io::Result<std::fs::File> {
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
fn verify_blob_root_identity(root: &Path, root_handle: &std::fs::File) -> std::io::Result<()> {
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
fn verify_blob_root_identity(root: &Path, _root_handle: &std::fs::File) -> std::io::Result<()> {
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
fn unlink_blob_shard_file_no_follow(
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
fn unlink_blob_shard_file_no_follow(
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
fn unlink_blob_shard_file_no_follow(
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
fn open_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
) -> std::io::Result<std::fs::File> {
    verify_blob_root_identity(root, root_handle)?;
    open_blob_shard_file_at_no_follow(root_handle, content_ref, libc::O_RDONLY)
}

#[cfg(windows)]
fn open_blob_shard_file_no_follow(
    root: &Path,
    root_handle: &std::fs::File,
    content_ref: &ContentRef,
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
fn open_blob_shard_file_no_follow(
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
fn openat_dir_no_follow(
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
fn openat_regular_file_no_follow(
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
fn open_or_create_dir_at_no_follow(
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
fn create_regular_file_at_no_follow(
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
fn unlink_entry_at(parent_fd: std::os::unix::io::RawFd, name: &str) -> std::io::Result<()> {
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
fn rename_entry_at(
    parent_fd: std::os::unix::io::RawFd,
    from: &str,
    to: &str,
) -> std::io::Result<()> {
    let c_from = std::ffi::CString::new(from)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let c_to = std::ffi::CString::new(to)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both names are NUL-terminated and relative to the same live
    // shard-directory handle.
    let rc = unsafe { libc::renameat(parent_fd, c_from.as_ptr(), parent_fd, c_to.as_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn available_space_at(root_handle: &std::fs::File) -> std::io::Result<u64> {
    use std::os::unix::io::AsRawFd;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `root_handle` is live and `stat` is a valid output buffer.
    let rc = unsafe { libc::fstatvfs(root_handle.as_raw_fd(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat.f_frsize.saturating_mul(u64::from(stat.f_bavail)))
}

#[cfg(unix)]
fn acquire_root_write_lock_at(root_handle: &std::fs::File) -> StorageResult<std::fs::File> {
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
fn blob_root_key(root: &Path) -> String {
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

/// Whether writing `required_write_bytes` more bytes to a volume currently
/// reporting `available` free bytes would leave it below `floor_bytes`.
///
/// Pure and filesystem-independent on purpose: the
/// exact boundary this guards — `available == floor_bytes + 1` must still
/// refuse a 2-byte write, because a floor-only check (`available <
/// floor_bytes`) does not account for the pending write's own size — is unit
/// tested directly against this function rather than against the real
/// filesystem's `fs4::available_space`, which fluctuates under concurrent
/// build/agent activity on a shared machine and made an earlier
/// exact-boundary integration test flaky. `saturating_sub` avoids underflow
/// when `required_write_bytes` exceeds `available` outright — that case
/// still correctly refuses for any nonzero floor.
fn crosses_floor(available: u64, required_write_bytes: u64, floor_bytes: u64) -> bool {
    available.saturating_sub(required_write_bytes) < floor_bytes
}

#[cfg(any(test, not(unix)))]
fn put_blocking_with_space_probe<F>(
    root: &Path,
    floor_bytes: u64,
    bytes: Vec<u8>,
    available_space: F,
) -> StorageResult<ContentRef>
where
    F: FnOnce(&Path) -> std::io::Result<u64>,
{
    let digest = blake3::hash(&bytes);
    let content_ref = ContentRef::from_digest_bytes(digest.as_bytes());
    let target = shard_path(root, &content_ref);

    // Content-addressed: identical bytes already on disk means this put is a
    // no-op (BlobStore::put's documented dedup contract) — skip the floor
    // check and the write entirely. The existing file's mtime is still
    // refreshed to now: a prior orphan re-published through this path
    // restarts its publish-grace clock exactly as a fresh write would,
    // rather than keeping a stale mtime that lets the orphan sweep delete it
    // out from under the caller's follow-up attachment write (khive#1313). The
    // caller already holds both the async and file-based publish advisory
    // locks for the duration of this call, so the refresh is serialized
    // against a concurrent sweep the same way an ordinary write is.
    if target.exists() {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .map_err(|e| map_io_err(e, "put_touch_open"))?;
        file.set_modified(SystemTime::now())
            .map_err(|e| map_io_err(e, "put_touch_mtime"))?;
        return Ok(content_ref);
    }

    let required_write_bytes = bytes.len() as u64;
    let available = available_space(root).map_err(|e| map_io_err(e, "put_check_space"))?;
    if crosses_floor(available, required_write_bytes, floor_bytes) {
        return Err(StorageError::CapacityFloor {
            capability: StorageCapability::Blob,
            volume: root.display().to_string(),
            available_bytes: available,
            floor_bytes,
        });
    }

    let shard_dir = target
        .parent()
        .expect("shard_path always nests under two directory levels");
    fs::create_dir_all(shard_dir).map_err(|e| map_io_err(e, "put_mkdir"))?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(shard_dir)
        .map_err(|e| map_io_err(e, "put_tempfile"))?;
    tmp.write_all(&bytes)
        .map_err(|e| map_io_err(e, "put_write"))?;
    tmp.flush().map_err(|e| map_io_err(e, "put_flush"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| map_io_err(e, "put_fsync"))?;

    let written_len = tmp
        .as_file()
        .metadata()
        .map_err(|e| map_io_err(e, "put_verify"))?
        .len();
    if written_len != bytes.len() as u64 {
        return Err(map_io_err(
            std::io::Error::other(format!(
                "temp file length {written_len} does not match {} written bytes",
                bytes.len()
            )),
            "put_verify",
        ));
    }

    tmp.persist(&target)
        .map_err(|e| map_io_err(e.error, "put_persist"))?;

    Ok(content_ref)
}

#[cfg(any(test, not(unix)))]
fn put_blocking(root: &Path, floor_bytes: u64, bytes: Vec<u8>) -> StorageResult<ContentRef> {
    let _root_write_guard = acquire_root_write_lock(root)?;
    put_blocking_with_space_probe(root, floor_bytes, bytes, |path| fs4::available_space(path))
}

fn acquire_root_write_lock_anchored(
    root: &Path,
    root_handle: &std::fs::File,
) -> StorageResult<std::fs::File> {
    verify_blob_root_identity(root, root_handle)
        .map_err(|e| map_io_err(e, "root_write_lock_identity"))?;
    #[cfg(unix)]
    {
        acquire_root_write_lock_at(root_handle)
    }
    #[cfg(not(unix))]
    {
        acquire_root_write_lock(root)
    }
}

#[cfg(unix)]
fn put_blocking_from_root_handle(
    root: &Path,
    root_handle: &std::fs::File,
    floor_bytes: u64,
    bytes: Vec<u8>,
) -> StorageResult<ContentRef> {
    use std::os::unix::io::AsRawFd;

    let _root_write_guard = acquire_root_write_lock_anchored(root, root_handle)?;
    let digest = blake3::hash(&bytes);
    let content_ref = ContentRef::from_digest_bytes(digest.as_bytes());

    // Preserve the dedup/republish contract while opening the existing leaf
    // relative to the retained root. A missing shard level and a missing leaf
    // are both the ordinary publish path; every other traversal failure is a
    // hard refusal.
    match open_blob_shard_file_at_no_follow(root_handle, &content_ref, libc::O_WRONLY) {
        Ok(file) => {
            file.set_modified(SystemTime::now())
                .map_err(|e| map_io_err(e, "put_touch_mtime"))?;
            return Ok(content_ref);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_io_err(error, "put_touch_open")),
    }

    let required_write_bytes = bytes.len() as u64;
    let available =
        available_space_at(root_handle).map_err(|e| map_io_err(e, "put_check_space"))?;
    if crosses_floor(available, required_write_bytes, floor_bytes) {
        return Err(StorageError::CapacityFloor {
            capability: StorageCapability::Blob,
            volume: root.display().to_string(),
            available_bytes: available,
            floor_bytes,
        });
    }

    let hex = content_ref.as_str();
    let shard1_dir = open_or_create_dir_at_no_follow(root_handle.as_raw_fd(), &hex[0..2])
        .map_err(|e| map_io_err(e, "put_mkdir"))?;
    let shard2_dir = open_or_create_dir_at_no_follow(shard1_dir.as_raw_fd(), &hex[2..4])
        .map_err(|e| map_io_err(e, "put_mkdir"))?;

    let temp_name = format!(".tmp-{}", Uuid::new_v4());
    let mut temp = create_regular_file_at_no_follow(shard2_dir.as_raw_fd(), &temp_name, 0o600)
        .map_err(|e| map_io_err(e, "put_tempfile"))?;
    let write_result = (|| -> StorageResult<()> {
        temp.write_all(&bytes)
            .map_err(|e| map_io_err(e, "put_write"))?;
        temp.flush().map_err(|e| map_io_err(e, "put_flush"))?;
        temp.sync_all().map_err(|e| map_io_err(e, "put_fsync"))?;

        let written_len = temp
            .metadata()
            .map_err(|e| map_io_err(e, "put_verify"))?
            .len();
        if written_len != bytes.len() as u64 {
            return Err(map_io_err(
                std::io::Error::other(format!(
                    "temp file length {written_len} does not match {} written bytes",
                    bytes.len()
                )),
                "put_verify",
            ));
        }
        Ok(())
    })();
    drop(temp);
    if let Err(error) = write_result {
        let _ = unlink_entry_at(shard2_dir.as_raw_fd(), &temp_name);
        return Err(error);
    }

    if let Err(error) = rename_entry_at(shard2_dir.as_raw_fd(), &temp_name, hex) {
        let _ = unlink_entry_at(shard2_dir.as_raw_fd(), &temp_name);
        return Err(map_io_err(error, "put_persist"));
    }
    Ok(content_ref)
}

#[cfg(not(unix))]
fn put_blocking_from_root_handle(
    root: &Path,
    root_handle: &std::fs::File,
    floor_bytes: u64,
    bytes: Vec<u8>,
) -> StorageResult<ContentRef> {
    verify_blob_root_identity(root, root_handle).map_err(|e| map_io_err(e, "put_root_identity"))?;
    put_blocking(root, floor_bytes, bytes)
}

#[cfg(any(test, not(unix)))]
fn acquire_root_write_lock(root: &Path) -> StorageResult<fs::File> {
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(ROOT_WRITE_LOCK_FILE))
        .map_err(|e| map_io_err(e, "root_write_lock_open"))?;
    fs4::FileExt::lock(&lock_file).map_err(|e| map_io_err(e, "root_write_lock_acquire"))?;
    Ok(lock_file)
}

fn database_gc_lock_path(database_path: &Path) -> PathBuf {
    let mut lock_path = database_path.as_os_str().to_os_string();
    lock_path.push(DATABASE_GC_LOCK_SUFFIX);
    PathBuf::from(lock_path)
}

fn acquire_database_gc_lock(database_path: Option<&Path>) -> StorageResult<Option<fs::File>> {
    let Some(database_path) = database_path else {
        // An in-memory database cannot be shared by another process. The
        // process-local lock below is therefore the complete owner fence.
        return Ok(None);
    };
    let lock_path = database_gc_lock_path(database_path);
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| map_io_err(e, "database_gc_lock_open"))?;
    fs4::FileExt::lock(&lock_file).map_err(|e| map_io_err(e, "database_gc_lock_acquire"))?;
    Ok(Some(lock_file))
}

#[cfg(all(unix, target_os = "macos"))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: `__error` returns this thread's errno cell; obtaining the
    // pointer has no preconditions beyond running on a thread, which every
    // caller here does.
    unsafe { libc::__error() }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: see the macOS arm above; `__errno_location` is the Linux/glibc
    // equivalent thread-local errno accessor.
    unsafe { libc::__errno_location() }
}

/// Zero `errno` on the current thread. `readdir` never clears `errno` itself
/// on success, so this must run immediately before each call for the
/// NULL-return ambiguity below to be resolvable afterward.
#[cfg(unix)]
fn clear_errno() {
    // SAFETY: `errno_location` returns a valid, live thread-local `c_int`
    // cell for the duration of this call.
    unsafe { *errno_location() = 0 };
}

#[cfg(unix)]
fn current_errno() -> libc::c_int {
    // SAFETY: see `clear_errno`.
    unsafe { *errno_location() }
}

/// List non-dot entry names of an open directory descriptor via `fdopendir`
/// on an INDEPENDENTLY reopened fd for the same directory (`openat(dir_fd,
/// ".", O_NOFOLLOW)`, never `dup(dir_fd)`) — the caller's descriptor stays
/// owned by the caller and, just as importantly, keeps its own read
/// position. `dup` shares the underlying open file description, and with it
/// the directory-stream position, with the fd it was duplicated from; a
/// persisted, repeatedly-listed handle (like `FsBlobStore::root_handle`)
/// would silently read as empty on its second call once a `dup`'d
/// `fdopendir`/`readdir` pass had already driven that shared position to
/// EOF. `openat(..., ".", ...)` yields a genuinely new open file
/// description, so this listing never perturbs the position of `dir_fd`
/// itself. `.`/`..` are excluded so a handle-relative walk built on this can
/// never step to a directory's parent or re-enter itself.
///
/// `readdir`'s NULL return is POSIX-ambiguous between end-of-stream and a
/// read error, distinguished only by `errno`: EOF leaves it unchanged (`0`,
/// since this loop always clears it first), an error sets it non-zero. This
/// walk clears `errno` before every `readdir` call and checks it on a NULL
/// return, so a mid-stream read error is reported as `Err` instead of
/// silently truncating the listing as if the stream had simply ended — the
/// distinction `transactional_orphan_sweep`'s candidate walk depends on to
/// avoid treating a partial, error-truncated scan as a complete one. The
/// `DIR*` stream is closed exactly once on every return path (`Ok`, the
/// mid-stream error path, and the pre-loop `fdopendir` failure).
#[cfg(unix)]
fn read_dir_names_no_follow(dir_fd: std::os::unix::io::RawFd) -> std::io::Result<Vec<String>> {
    use std::os::unix::io::IntoRawFd;

    let reopened = openat_dir_no_follow(dir_fd, ".")?;
    // SAFETY: `reopened` was just opened above and is uniquely owned; its
    // raw fd is handed to `fdopendir`, which takes ownership of it on
    // success (and closes it via `closedir` below).
    let owned_fd = reopened.into_raw_fd();
    // SAFETY: `owned_fd` is valid and uniquely owned; `fdopendir` takes
    // ownership of it on success.
    let dirp = unsafe { libc::fdopendir(owned_fd) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        // SAFETY: `owned_fd` is still owned by us since `fdopendir` failed.
        unsafe { libc::close(owned_fd) };
        return Err(err);
    }
    let mut names = Vec::new();
    loop {
        // `readdir` leaves `errno` untouched on EOF; clearing it here is
        // what makes that observable as distinct from an error below.
        clear_errno();
        // SAFETY: `dirp` is a valid, open `DIR*` for this whole loop.
        let entry = unsafe { libc::readdir(dirp) };
        if entry.is_null() {
            if current_errno() != 0 {
                let err = std::io::Error::last_os_error();
                // SAFETY: `dirp` is still open and owned by this call; this
                // is the one closedir on the error path, matching the one
                // closedir on the `Ok` path below.
                unsafe { libc::closedir(dirp) };
                return Err(err);
            }
            break;
        }
        // SAFETY: `d_name` is NUL-terminated, so its first byte is always
        // in bounds; `.`/`..` (and any other dot-leading entry, e.g. an
        // in-flight `.tmp-*` file or the root write-lock file) are rejected
        // on this raw byte before any allocation happens for them.
        let first = unsafe { *(*entry).d_name.as_ptr() };
        if first == b'.' as libc::c_char {
            continue;
        }
        // SAFETY: `entry` is valid until the next `readdir`/`closedir`
        // call; the name is copied out before either.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        names.push(name);
    }
    // SAFETY: `dirp` was successfully opened above and not yet closed.
    unsafe { libc::closedir(dirp) };
    Ok(names)
}

/// Enumerate orphan-sweep candidates and their mtimes entirely relative to
/// the retained `root_handle` — no path is ever re-resolved from `root` for
/// this walk, so a concurrent replacement of the root or a shard directory
/// cannot redirect candidate enumeration or the mtime read used for
/// `within_publish_grace` classification outside the retained root.
///
/// Each shard level is opened with `openat(..., O_NOFOLLOW)` relative to the
/// previous, already-verified descriptor (same helpers `put`/`get`/`delete`
/// use), so a symlink planted at either shard level is refused rather than
/// followed and its contents skipped, never swept. A candidate's mtime is
/// read via `fstat` on the handle returned by opening the leaf with
/// `openat(..., O_NOFOLLOW)`, never via `fs::metadata` on a path. If the
/// leaf cannot be opened as a verified regular file it is dropped entirely
/// (nothing to protect — it is not a delete candidate); if it opens but its
/// metadata/mtime cannot be read, it is kept as a candidate with an unknown
/// mtime, which `within_publish_grace` treats as protected — the safe
/// direction for a sweep that only ever destroys data.
#[cfg(unix)]
fn walk_blob_files_from_root_handle(
    root_handle: &std::fs::File,
    // Only used under `#[cfg(test)]`, to key `walk_leaf_sync_hook` by this
    // walk's canonical root; underscore-prefixed so non-test builds don't
    // warn on the unused parameter.
    _root: &Path,
) -> std::io::Result<Vec<(ContentRef, Option<SystemTime>)>> {
    use std::os::unix::io::AsRawFd;

    let mut out = Vec::new();
    for l1_name in read_dir_names_no_follow(root_handle.as_raw_fd())? {
        let l1_dir = match openat_dir_no_follow(root_handle.as_raw_fd(), &l1_name) {
            Ok(dir) => dir,
            Err(_) => continue,
        };
        for l2_name in read_dir_names_no_follow(l1_dir.as_raw_fd())? {
            let l2_dir = match openat_dir_no_follow(l1_dir.as_raw_fd(), &l2_name) {
                Ok(dir) => dir,
                Err(_) => continue,
            };
            for leaf_name in read_dir_names_no_follow(l2_dir.as_raw_fd())? {
                // Non-hex names never round-trip through `ContentRef`;
                // orphan_sweep only ever acts on names that do.
                let Ok(content_ref) = ContentRef::from_hex(leaf_name.clone()) else {
                    continue;
                };
                #[cfg(test)]
                if let Some(hook) = walk_leaf_sync_hook::take(_root) {
                    let _ = hook.reached.send(());
                    let _ = hook.release.recv();
                }
                let file = match openat_regular_file_no_follow(
                    l2_dir.as_raw_fd(),
                    &leaf_name,
                    libc::O_RDONLY,
                ) {
                    Ok(file) => file,
                    Err(_) => continue,
                };
                let mtime = file.metadata().ok().and_then(|meta| meta.modified().ok());
                out.push((content_ref, mtime));
            }
        }
    }
    Ok(out)
}

/// Non-Unix tier has no descriptor-relative directory-listing API in this
/// codebase (see the equivalent fail-closed note on `unlink_blob_shard_file_no_follow`'s
/// `not(any(unix, windows))` arm). Rather than fall back to path-based
/// `fs::read_dir`/`fs::metadata` reads — which reintroduces exactly the
/// TOCTOU this function exists to close — candidate enumeration refuses
/// outright, and `transactional_orphan_sweep` surfaces that as a sweep
/// failure instead of classifying anything.
#[cfg(not(unix))]
fn walk_blob_files_from_root_handle(
    _root_handle: &std::fs::File,
    _root: &Path,
) -> std::io::Result<Vec<(ContentRef, Option<SystemTime>)>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "orphan sweep candidate enumeration requires descriptor-relative directory \
         reads, available only on unix in this release; refusing to classify via \
         path-based reads",
    ))
}

/// Whether a candidate file is still inside its publish grace period and must
/// be left alone regardless of liveness.
///
/// `put`'s two-step client protocol (bytes land first, a *later* attachment write
/// commits the `content_ref`) means a blob can be physically on disk with
/// zero live references for a window entirely outside this store's control —
/// the referencing write simply hasn't happened yet. A file whose mtime is
/// younger than `grace_period` is therefore treated as not-yet-orphaned: an
/// unreadable mtime (removed mid-scan, clock weirdness) is treated the same
/// way (age unknown -> protect it), the safe direction for a sweep that only
/// ever destroys data.
fn within_publish_grace(
    mtime: Option<SystemTime>,
    now: SystemTime,
    grace_period: Duration,
) -> bool {
    let age = mtime.and_then(|mtime| now.duration_since(mtime).ok());
    match age {
        Some(age) => age < grace_period,
        None => true,
    }
}

#[derive(Debug)]
struct PreparedTransactionalSweep {
    result: BlobOrphanSweepResult,
    candidates: Vec<(ContentRef, bool)>,
}

/// Perform every filesystem-dependent part of candidate classification before
/// SQLite's writer transaction opens.
fn prepare_transactional_sweep(
    files: Vec<(ContentRef, Option<SystemTime>)>,
    grace_period: Duration,
) -> PreparedTransactionalSweep {
    let now = SystemTime::now();
    let mut result = BlobOrphanSweepResult::default();
    let mut candidates = Vec::with_capacity(files.len());
    for (content_ref, mtime) in files {
        result.scanned += 1;
        let within_grace = within_publish_grace(mtime, now, grace_period);
        candidates.push((content_ref, within_grace));
    }
    PreparedTransactionalSweep { result, candidates }
}

#[derive(Debug)]
struct BlobGcBatchRows {
    grace_period_skipped: u64,
    would_delete: u64,
    claimed_rows: Vec<SqlRow>,
}

fn required_nonnegative_count(
    value: Option<SqlValue>,
    operation: &'static str,
) -> StorageResult<u64> {
    match value {
        Some(SqlValue::Integer(value)) if value >= 0 => Ok(value as u64),
        other => Err(StorageError::Internal(format!(
            "{operation} returned an invalid count: {other:?}"
        ))),
    }
}

fn invalid_content_ref(message: String) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Blob,
        operation: "transactional_orphan_sweep".into(),
        message,
    }
}

/// Whether this database carries the complete V21 attachment-only GC fencing
/// set and durable completed cutover marker.
///
/// `transactional_orphan_sweep` is reachable from any `SqlAccess` a caller
/// hands it, including a `StorageBackend` constructed directly (e.g.
/// `StorageBackend::memory()`/`sqlite()` used without `prepare_core_schema`)
/// that never ran core migrations. The triggers are the fence that keeps a
/// concurrent attachment write from resurrecting a claimed digest in the
/// released-writer window, so a database missing any element of the set
/// cannot satisfy the fail-closed guarantee the
/// [`BlobStore::transactional_orphan_sweep`] contract requires; the sweep
/// refuses with [`StorageError::Unsupported`] rather than degrading to
/// unfenced deletion.
async fn blob_gc_fencing_complete(sql: &dyn SqlAccess) -> StorageResult<bool> {
    let mut reader = sql.reader().await?;
    let present = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM sqlite_master \
                      WHERE (type = 'table' AND name IN ( \
                                 'blob_gc_claims', 'attachments', \
                                 'attachment_cutover_state')) \
                         OR (type = 'index' AND name IN ( \
                             'idx_blob_gc_claims_content_ref', \
                             'idx_attachments_content_ref')) \
                         OR (type = 'trigger' AND name IN ( \
                             'attachments_reject_claimed_blob_insert', \
                             'attachments_reject_claimed_blob_update'))"
                    .to_string(),
                params: vec![],
                label: Some("blob_gc_fencing_complete".to_string()),
            })
            .await?,
        "blob_gc_fencing_complete",
    )?;
    if present != 7 {
        return Ok(false);
    }

    let legacy_objects = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT \
                        (SELECT COUNT(*) FROM pragma_table_info('entities') \
                         WHERE name = 'content_ref') \
                      + (SELECT COUNT(*) FROM sqlite_master \
                         WHERE (type = 'index' AND name = 'idx_entities_content_ref') \
                            OR (type = 'trigger' AND name IN ( \
                                'entities_reject_claimed_blob_insert', \
                                'entities_reject_claimed_blob_update')))"
                    .to_string(),
                params: vec![],
                label: Some("blob_gc_legacy_fencing_absent".to_string()),
            })
            .await?,
        "blob_gc_legacy_fencing_absent",
    )?;
    if legacy_objects != 0 {
        return Ok(false);
    }

    let complete = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachment_cutover_state AS cutover \
                      WHERE cutover.singleton = 1 \
                        AND cutover.state = 'complete' \
                        AND cutover.completed_at IS NOT NULL \
                        AND (SELECT COUNT(*) FROM _schema_migrations \
                             WHERE version = 21 \
                               AND name = 'attachments_first_class') = 1 \
                        AND (SELECT MAX(version) FROM _schema_migrations) = 21"
                    .to_string(),
                params: vec![],
                label: Some("blob_gc_cutover_complete".to_string()),
            })
            .await?,
        "blob_gc_cutover_complete",
    )?;
    Ok(complete == 1)
}

fn unsupported_blob_gc_epoch() -> StorageError {
    StorageError::Unsupported {
        capability: StorageCapability::Blob,
        operation: "transactional_orphan_sweep".into(),
        message: "transactional blob GC requires a complete V21 attachment cutover with \
                  the attachment claim-fencing set; refusing both report-only and \
                  destructive sweep in this database epoch"
            .into(),
    }
}

/// The sentinel digest the fence probe claims. All zeros is canonical-form
/// valid (64 lowercase hex) and unreachable as a real BLAKE3 digest for any
/// stored object in practice; probe rows never survive the probe transaction.
const BLOB_GC_FENCE_PROBE_REF: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The RAISE(ABORT) message shared by the V20 and V21 fencing triggers. The probe
/// requires the rejection to be OUR fence, not an incidental failure.
const BLOB_GC_FENCE_TRIGGER_MESSAGE: &str = "content_ref is reserved by an active blob sweep";

/// Prove the V21 fence actually fences, not merely that objects with the
/// right NAMES exist in `sqlite_master`. Same-named no-op triggers (or a
/// rewritten trigger body) would pass the name census while letting a
/// claimed `content_ref` become live in the released-writer window, so the
/// gate exercises the fence: inside one writer transaction it claims the
/// all-zero sentinel AND a second random digest, and attempts the attachment
/// INSERT and attachment UPDATE the triggers must reject for EACH claimed
/// digest — with the second digest's arms using a different attachment shape
/// (substrate `note`, role `evidence`), so a trigger rewrite restricted to
/// one digest, substrate, or role fails an arm instead of passing a
/// fixed-sentinel census. Every arm must fail with the triggers' own RAISE
/// message, and every probe row is deleted before the unit commits. Any
/// other outcome — a write accepted, or rejected for a different reason —
/// refuses the sweep with [`StorageError::Unsupported`].
async fn blob_gc_fence_probe(sql: &dyn SqlAccess) -> StorageResult<()> {
    let run = Uuid::new_v4().simple().to_string();
    blob_gc_fence_probe_with_ids(
        sql,
        format!("__blob-gc-fence-probe-insert-{run}__"),
        format!("__blob-gc-fence-probe-update-{run}__"),
        format!("__blob-gc-fence-probe-insert2-{run}__"),
        format!("__blob-gc-fence-probe-update2-{run}__"),
        format!("__fence_probe-{run}__"),
    )
    .await
}

/// Probe body with explicit row ids so tests can force an id collision.
/// Production callers go through [`blob_gc_fence_probe`], which mints
/// per-run random ids; the guard below still refuses to run — touching
/// nothing — if any minted id already names a row.
async fn blob_gc_fence_probe_with_ids(
    sql: &dyn SqlAccess,
    insert_id: String,
    update_id: String,
    insert2_id: String,
    update2_id: String,
    claim_key: String,
) -> StorageResult<()> {
    fn fence_rejection(result: Result<u64, StorageError>) -> Result<bool, String> {
        match result {
            Ok(_) => Ok(false),
            Err(error) => {
                let text = error.to_string();
                if text.contains(BLOB_GC_FENCE_TRIGGER_MESSAGE) {
                    Ok(true)
                } else {
                    Err(text)
                }
            }
        }
    }

    fn required_seed(value: Option<SqlValue>) -> StorageResult<String> {
        match value {
            Some(SqlValue::Text(seed)) => Ok(seed),
            _ => Err(StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation: "transactional_orphan_sweep".into(),
                message: "the blob GC fence probe could not select an unclaimed \
                          canonical seed; refusing deletion so a later sweep can retry"
                    .into(),
            }),
        }
    }

    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            // Ownership guard: the cleanup below deletes these ids
            // unconditionally, so the probe may only proceed when it can
            // prove every id is unclaimed in EVERY table cleanup touches.
            let preexisting = writer
                .query_row(SqlStatement {
                    sql: "SELECT (SELECT COUNT(*) FROM attachments \
                                   WHERE record_uuid IN (?1, ?2, ?3, ?4)) \
                              + (SELECT COUNT(*) FROM blob_gc_claims WHERE root_key = ?5)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert_id.clone()),
                        SqlValue::Text(update_id.clone()),
                        SqlValue::Text(insert2_id.clone()),
                        SqlValue::Text(update2_id.clone()),
                        SqlValue::Text(claim_key.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_ownership_guard".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|c| c.value.clone()));
            match preexisting {
                Some(SqlValue::Integer(0)) => {}
                Some(SqlValue::Integer(_)) => {
                    return Err(StorageError::Unsupported {
                        capability: StorageCapability::Blob,
                        operation: "transactional_orphan_sweep".into(),
                        message: "the blob GC fence probe's row ids collide with existing \
                                  rows; refusing to probe rather than delete data the \
                                  probe does not own"
                            .into(),
                    });
                }
                _ => {
                    return Err(StorageError::Internal(
                        "blob GC fence probe ownership guard returned no count".into(),
                    ));
                }
            }

            // The UPDATE arm needs a valid, initially unclaimed reference.
            // Select it under this same writer transaction instead of using a
            // fixed sentinel that a recoverable abandoned claim could fence
            // forever. Eight fresh candidates keep collision handling bounded;
            // no candidate means a safe, retryable refusal.
            let seed_ref = writer
                .query_row(SqlStatement {
                    sql: "WITH RECURSIVE candidates(attempt, content_ref) AS ( \
                              SELECT 1, lower(hex(randomblob(32))) \
                              UNION ALL \
                              SELECT attempt + 1, lower(hex(randomblob(32))) \
                              FROM candidates WHERE attempt < 8 \
                          ) \
                          SELECT candidate.content_ref FROM candidates AS candidate \
                          WHERE candidate.content_ref <> ?1 \
                            AND NOT EXISTS ( \
                                SELECT 1 FROM blob_gc_claims \
                                WHERE content_ref = candidate.content_ref \
                            ) \
                          LIMIT 1"
                        .to_string(),
                    params: vec![SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string())],
                    label: Some("blob_gc_fence_probe_select_seed".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|column| column.value.clone()));
            let seed_ref = required_seed(seed_ref)?;

            let seed2_ref = writer
                .query_row(SqlStatement {
                    sql: "WITH RECURSIVE candidates(attempt, content_ref) AS ( \
                              SELECT 1, lower(hex(randomblob(32))) \
                              UNION ALL \
                              SELECT attempt + 1, lower(hex(randomblob(32))) \
                              FROM candidates WHERE attempt < 8 \
                          ) \
                          SELECT candidate.content_ref FROM candidates AS candidate \
                          WHERE candidate.content_ref NOT IN (?1, ?2) \
                            AND NOT EXISTS ( \
                                SELECT 1 FROM blob_gc_claims \
                                WHERE content_ref = candidate.content_ref \
                            ) \
                          LIMIT 1"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(seed_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_select_seed2".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|column| column.value.clone()));
            let seed2_ref = required_seed(seed2_ref)?;

            // The second CLAIMED digest. A trigger rewrite conditioned on the
            // fixed all-zero sentinel passes that sentinel's arms; this digest
            // is random per run, so such a rewrite fails the arms below.
            let probe2_ref = writer
                .query_row(SqlStatement {
                    sql: "WITH RECURSIVE candidates(attempt, content_ref) AS ( \
                              SELECT 1, lower(hex(randomblob(32))) \
                              UNION ALL \
                              SELECT attempt + 1, lower(hex(randomblob(32))) \
                              FROM candidates WHERE attempt < 8 \
                          ) \
                          SELECT candidate.content_ref FROM candidates AS candidate \
                          WHERE candidate.content_ref NOT IN (?1, ?2, ?3) \
                            AND NOT EXISTS ( \
                                SELECT 1 FROM blob_gc_claims \
                                WHERE content_ref = candidate.content_ref \
                            ) \
                          LIMIT 1"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(seed_ref.clone()),
                        SqlValue::Text(seed2_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_select_probe2".to_string()),
                })
                .await?
                .and_then(|row| row.columns.first().map(|column| column.value.clone()));
            let probe2_ref = required_seed(probe2_ref)?;

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                          VALUES (?1, ?2, 0), (?1, ?3, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(claim_key.clone()),
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(probe2_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_claim".to_string()),
                })
                .await?;

            let insert_attempt = writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'entity', 'content', ?2, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert_id.clone()),
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                    ],
                    label: Some("blob_gc_fence_probe_insert_arm".to_string()),
                })
                .await;
            let insert_fenced = fence_rejection(insert_attempt);

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'entity', 'content', ?2, 0)"
                        .to_string(),
                    params: vec![SqlValue::Text(update_id.clone()), SqlValue::Text(seed_ref)],
                    label: Some("blob_gc_fence_probe_update_arm_seed".to_string()),
                })
                .await?;
            let update_attempt = writer
                .execute(SqlStatement {
                    sql: "UPDATE attachments SET content_ref = ?1 \
                          WHERE record_uuid = ?2 AND role = 'content'"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(BLOB_GC_FENCE_PROBE_REF.to_string()),
                        SqlValue::Text(update_id.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_update_arm".to_string()),
                })
                .await;
            let update_fenced = fence_rejection(update_attempt);

            let insert2_attempt = writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'note', 'evidence', ?2, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert2_id.clone()),
                        SqlValue::Text(probe2_ref.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_insert2_arm".to_string()),
                })
                .await;
            let insert2_fenced = fence_rejection(insert2_attempt);

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO attachments \
                          (record_uuid, substrate, role, content_ref, created_at) \
                          VALUES (?1, 'note', 'evidence', ?2, 0)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(update2_id.clone()),
                        SqlValue::Text(seed2_ref),
                    ],
                    label: Some("blob_gc_fence_probe_update2_arm_seed".to_string()),
                })
                .await?;
            let update2_attempt = writer
                .execute(SqlStatement {
                    sql: "UPDATE attachments SET content_ref = ?1 \
                          WHERE record_uuid = ?2 AND role = 'evidence'"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(probe2_ref),
                        SqlValue::Text(update2_id.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_update2_arm".to_string()),
                })
                .await;
            let update2_fenced = fence_rejection(update2_attempt);

            // Remove every probe row before this unit commits, including an
            // attachment row a dead fence let through.
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM attachments WHERE record_uuid IN (?1, ?2, ?3, ?4)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(insert_id.clone()),
                        SqlValue::Text(update_id.clone()),
                        SqlValue::Text(insert2_id.clone()),
                        SqlValue::Text(update2_id.clone()),
                    ],
                    label: Some("blob_gc_fence_probe_cleanup_attachments".to_string()),
                })
                .await?;
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM blob_gc_claims WHERE root_key = ?1".to_string(),
                    params: vec![SqlValue::Text(claim_key)],
                    label: Some("blob_gc_fence_probe_cleanup_claim".to_string()),
                })
                .await?;

            Ok(
                Box::new((insert_fenced, update_fenced, insert2_fenced, update2_fenced))
                    as Box<dyn std::any::Any + Send>,
            )
        })
    });
    let outcome = sql.atomic_unit(op).await?;
    let (insert_fenced, update_fenced, insert2_fenced, update2_fenced) = *outcome
        .downcast::<(
            Result<bool, String>,
            Result<bool, String>,
            Result<bool, String>,
            Result<bool, String>,
        )>()
        .map_err(|_| {
            StorageError::Internal("blob GC fence probe returned an unexpected outcome type".into())
        })?;
    let arm_verdict = |arm: &str, fenced: Result<bool, String>| -> StorageResult<()> {
        match fenced {
            Ok(true) => Ok(()),
            Ok(false) => Err(StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation: "transactional_orphan_sweep".into(),
                message: format!(
                    "the V21 fencing triggers exist by name but did not reject a claimed \
                     content_ref on the attachment {arm} path; refusing unfenced deletion"
                ),
            }),
            Err(other) => Err(StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation: "transactional_orphan_sweep".into(),
                message: format!(
                    "the blob GC fence probe could not verify the attachment {arm} fence \
                     (unexpected rejection: {other}); refusing unfenced deletion"
                ),
            }),
        }
    };
    arm_verdict("INSERT", insert_fenced)?;
    arm_verdict("UPDATE", update_fenced)?;
    arm_verdict("second-digest INSERT", insert2_fenced)?;
    arm_verdict("second-digest UPDATE", update2_fenced)
}

async fn validate_blob_gc_evidence(sql: &dyn SqlAccess) -> StorageResult<()> {
    // These full-table integrity probes are statement-scoped reads. Keep them
    // off the single writer; only their one-row result is materialized. The
    // database sweep owner excludes another claim producer, and each bounded
    // claim unit anti-joins the then-current live rows under its writer lock.
    let mut reader = sql.reader().await?;
    // length() and GLOB both stop at an embedded NUL, so a value of 64 hex
    // characters followed by a NUL and arbitrary bytes passes them while
    // failing the exact-equality liveness anti-join. The byte-length arm
    // closes that class: chars = 64 AND bytes = 64 * the encoding's bytes
    // per ASCII character forces a NUL-free canonical value. CAST(TEXT AS
    // BLOB) yields the database text encoding's bytes (1 per hex char in
    // UTF-8, 2 in UTF-16), so the width is derived from the same database
    // rather than assumed, and an unrecognizable answer fails closed.
    let canonical_bytes = match reader
        .query_row(SqlStatement {
            sql: "SELECT length(CAST('x' AS BLOB))".to_string(),
            params: vec![],
            label: Some("blob_gc_validate_encoding_width".to_string()),
        })
        .await?
        .and_then(|row| row.columns.first().map(|column| column.value.clone()))
    {
        Some(SqlValue::Integer(width)) if (1..=4).contains(&width) => width * 64,
        other => {
            return Err(invalid_content_ref(format!(
                "the text-encoding width probe returned {other:?}; refusing GC validation"
            )));
        }
    };
    let invalid_claim = reader
        .query_row(SqlStatement {
            sql: "SELECT content_ref FROM blob_gc_claims \
                  WHERE typeof(content_ref) <> 'text' \
                     OR length(content_ref) <> 64 \
                     OR length(CAST(content_ref AS BLOB)) <> ?1 \
                     OR content_ref GLOB '*[^0-9a-f]*' \
                  LIMIT 1"
                .to_string(),
            params: vec![SqlValue::Integer(canonical_bytes)],
            label: Some("blob_gc_validate_existing_claims".to_string()),
        })
        .await?;
    if invalid_claim.is_some() {
        return Err(invalid_content_ref(
            "blob_gc_claims.content_ref contained a non-canonical value".into(),
        ));
    }

    let invalid_live = reader
        .query_row(SqlStatement {
            sql: "SELECT content_ref FROM attachments \
                  WHERE typeof(content_ref) <> 'text' \
                      OR length(content_ref) <> 64 \
                      OR length(CAST(content_ref AS BLOB)) <> ?1 \
                      OR content_ref GLOB '*[^0-9a-f]*' \
                  LIMIT 1"
                .to_string(),
            params: vec![SqlValue::Integer(canonical_bytes)],
            label: Some("blob_gc_validate_live_refs".to_string()),
        })
        .await?;
    if invalid_live.is_some() {
        return Err(invalid_content_ref(
            "attachments.content_ref contained a non-canonical value".into(),
        ));
    }
    Ok(())
}

async fn release_abandoned_blob_gc_claim_batch(sql: &dyn SqlAccess) -> StorageResult<u64> {
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let released = writer
                .execute(SqlStatement {
                    sql: "DELETE FROM blob_gc_claims \
                          WHERE rowid IN ( \
                            SELECT rowid FROM blob_gc_claims \
                            ORDER BY rowid LIMIT ?1 \
                          )"
                    .to_string(),
                    params: vec![SqlValue::Integer(BLOB_GC_CLAIM_BATCH_SIZE as i64)],
                    label: Some("blob_gc_release_abandoned_claim_batch".to_string()),
                })
                .await?;
            Ok(Box::new(released) as Box<dyn std::any::Any + Send>)
        })
    });
    let released = sql.atomic_unit(op).await?;
    released.downcast::<u64>().map(|count| *count).map_err(|_| {
        StorageError::Internal(
            "transactional orphan sweep returned an unexpected recovery count type".into(),
        )
    })
}

async fn claim_blob_gc_batch(
    sql: &dyn SqlAccess,
    root_key: String,
    candidates: &[(ContentRef, bool)],
    dry_run: bool,
) -> StorageResult<BlobGcBatchRows> {
    debug_assert!(candidates.len() <= BLOB_GC_CLAIM_BATCH_SIZE);
    let eligible_refs = candidates
        .iter()
        .filter(|(_, within_grace)| !within_grace)
        .map(|(content_ref, _)| content_ref.to_string())
        .collect::<Vec<_>>();
    let grace_refs = candidates
        .iter()
        .filter(|(_, within_grace)| *within_grace)
        .map(|(content_ref, _)| content_ref.to_string())
        .collect::<Vec<_>>();
    let eligible_json = serde_json::to_string(&eligible_refs).map_err(|error| {
        StorageError::Internal(format!(
            "failed to prepare blob GC eligible candidate batch: {error}"
        ))
    })?;
    let grace_json = serde_json::to_string(&grace_refs).map_err(|error| {
        StorageError::Internal(format!(
            "failed to prepare blob GC grace candidate batch: {error}"
        ))
    })?;
    let claimed_at = chrono::Utc::now().timestamp_micros();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let grace_period_skipped = required_nonnegative_count(
                writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                              WHERE NOT EXISTS ( \
                                SELECT 1 FROM attachments \
                                WHERE content_ref = candidate.value \
                              )"
                        .to_string(),
                        params: vec![SqlValue::Text(grace_json)],
                        label: Some("blob_gc_count_grace_candidates_batch".to_string()),
                    })
                    .await?,
                "blob_gc_count_grace_candidates_batch",
            )?;

            if dry_run {
                let would_delete = required_nonnegative_count(
                    writer
                        .query_scalar(SqlStatement {
                            sql: "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                                  WHERE NOT EXISTS ( \
                                    SELECT 1 FROM attachments \
                                    WHERE content_ref = candidate.value \
                                  )"
                            .to_string(),
                            params: vec![SqlValue::Text(eligible_json)],
                            label: Some("blob_gc_count_dry_run_candidates_batch".to_string()),
                        })
                        .await?,
                    "blob_gc_count_dry_run_candidates_batch",
                )?;
                return Ok(Box::new(BlobGcBatchRows {
                    grace_period_skipped,
                    would_delete,
                    claimed_rows: Vec::new(),
                }) as Box<dyn std::any::Any + Send>);
            }

            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                          SELECT ?1, candidate.value, ?3 \
                          FROM json_each(?2) AS candidate \
                          WHERE NOT EXISTS ( \
                            SELECT 1 FROM attachments \
                            WHERE content_ref = candidate.value \
                          )"
                    .to_string(),
                    params: vec![
                        SqlValue::Text(root_key.clone()),
                        SqlValue::Text(eligible_json),
                        SqlValue::Integer(claimed_at),
                    ],
                    label: Some("blob_gc_claim_candidate_batch".to_string()),
                })
                .await?;

            let claimed_rows = writer
                .query_all(SqlStatement {
                    sql: "SELECT content_ref FROM blob_gc_claims \
                          WHERE root_key = ?1 ORDER BY content_ref"
                        .to_string(),
                    params: vec![SqlValue::Text(root_key)],
                    label: Some("blob_gc_claimed_candidate_batch".to_string()),
                })
                .await?;
            Ok(Box::new(BlobGcBatchRows {
                grace_period_skipped,
                would_delete: claimed_rows.len() as u64,
                claimed_rows,
            }) as Box<dyn std::any::Any + Send>)
        })
    });
    let rows = sql.atomic_unit(op).await?;
    rows.downcast::<BlobGcBatchRows>()
        .map(|rows| *rows)
        .map_err(|_| {
            StorageError::Internal(
                "transactional orphan sweep returned an unexpected batch-row type".into(),
            )
        })
}

fn parse_blob_gc_claim_rows(rows: Vec<SqlRow>) -> StorageResult<Vec<ContentRef>> {
    let mut claimed = Vec::with_capacity(rows.len());
    for row in rows {
        let raw = match row.get("content_ref") {
            Some(SqlValue::Text(raw)) => raw.clone(),
            _ => {
                return Err(invalid_content_ref(
                    "blob_gc_claims.content_ref contained a non-text value".into(),
                ));
            }
        };
        claimed.push(ContentRef::from_hex(raw).map_err(invalid_content_ref)?);
    }
    Ok(claimed)
}

async fn release_blob_gc_batch(sql: &dyn SqlAccess, root_key: String) -> StorageResult<()> {
    let cleanup: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM blob_gc_claims WHERE root_key = ?1".to_string(),
                    params: vec![SqlValue::Text(root_key)],
                    label: Some("blob_gc_release_claim_batch".to_string()),
                })
                .await?;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });
    sql.atomic_unit(cleanup).await?;
    Ok(())
}

/// Process-wide database owner fence for transactional blob sweeps.
///
/// Claims live in the database and their attachment triggers are database-global,
/// so a root-only lock is insufficient: two differently configured roots for
/// one database must not recover each other's live claims. File-backed pools
/// additionally take [`acquire_database_gc_lock`] for cross-process exclusion.
type SweepLockMap = HashMap<Option<PathBuf>, Arc<DatabaseGcProcessLock>>;

#[derive(Debug, Default)]
struct DatabaseGcProcessLock {
    held: StdMutex<bool>,
    released: std::sync::Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

impl DatabaseGcProcessLock {
    fn acquire(self: &Arc<Self>) -> DatabaseGcProcessGuard {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *held {
            #[cfg(test)]
            self.waiters
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held = self
                .released
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(test)]
            self.waiters
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        *held = true;
        DatabaseGcProcessGuard {
            lock: Arc::clone(self),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<DatabaseGcProcessGuard> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *held {
            return None;
        }
        *held = true;
        Some(DatabaseGcProcessGuard {
            lock: Arc::clone(self),
        })
    }
}

#[derive(Debug)]
struct DatabaseGcProcessGuard {
    lock: Arc<DatabaseGcProcessLock>,
}

impl Drop for DatabaseGcProcessGuard {
    fn drop(&mut self) {
        let mut held = self
            .lock
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(*held, "database GC process owner released twice");
        *held = false;
        self.lock.released.notify_one();
    }
}

fn database_sweep_locks() -> &'static StdMutex<SweepLockMap> {
    static REGISTRY: OnceLock<StdMutex<SweepLockMap>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn sweep_lock_for_database(database_path: Option<&Path>) -> Arc<DatabaseGcProcessLock> {
    let key = database_path.map(Path::to_path_buf);
    let mut locks = database_sweep_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(DatabaseGcProcessLock::default()))
        .clone()
}

#[cfg(test)]
pub(crate) fn database_gc_waiter_count(database_path: Option<&Path>) -> usize {
    sweep_lock_for_database(database_path)
        .waiters
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// Exclusive canonical-database ownership shared by transactional blob GC and
/// the boot-gated V21 attachment cutover.
///
/// The process-local mutex is acquired first and retained while the advisory
/// file lock is acquired on a blocking thread. Moving the owned mutex guard
/// into that closure makes cancellation safe: dropping the outer future cannot
/// release process ownership while a blocking advisory acquisition continues.
pub struct DatabaseGcOwnerGuard {
    _process_guard: DatabaseGcProcessGuard,
    _advisory_guard: Option<fs::File>,
    database_path: Option<PathBuf>,
}

pub(crate) fn acquire_database_gc_owner_for_path_blocking(
    database_path: Option<PathBuf>,
) -> StorageResult<DatabaseGcOwnerGuard> {
    let process_guard = sweep_lock_for_database(database_path.as_deref()).acquire();
    let advisory_guard = acquire_database_gc_lock(database_path.as_deref())?;
    Ok(DatabaseGcOwnerGuard {
        _process_guard: process_guard,
        _advisory_guard: advisory_guard,
        database_path,
    })
}

/// Try to acquire canonical database-GC ownership without waiting.
///
/// This is the fail-closed bridge for the legacy raw-connection migration API:
/// a caller may already hold an opaque pooled writer guard, so waiting here
/// could invert the canonical owner-before-writer order used by sweeps. The
/// production backend boot path uses the blocking helper before writer
/// checkout instead.
pub(crate) fn try_acquire_database_gc_owner_for_path(
    database_path: PathBuf,
) -> StorageResult<DatabaseGcOwnerGuard> {
    let process_guard = sweep_lock_for_database(Some(&database_path))
        .try_acquire()
        .ok_or_else(|| {
            StorageError::Internal(format!(
                "database GC owner for {} is already held; retry schema migration through the \
                 coordinated backend boot path",
                database_path.display()
            ))
        })?;
    let lock_path = database_gc_lock_path(&database_path);
    let advisory_guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| map_io_err(error, "database_gc_lock_open"))?;
    fs4::FileExt::try_lock(&advisory_guard)
        .map_err(|error| map_io_err(error.into(), "database_gc_lock_try_acquire"))?;
    Ok(DatabaseGcOwnerGuard {
        _process_guard: process_guard,
        _advisory_guard: Some(advisory_guard),
        database_path: Some(database_path),
    })
}

impl std::fmt::Debug for DatabaseGcOwnerGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseGcOwnerGuard")
            .field("database_path", &self.database_path)
            .finish_non_exhaustive()
    }
}

impl DatabaseGcOwnerGuard {
    /// Canonical database path that keys this owner, or `None` for an
    /// in-memory database whose process-local mutex is the complete fence.
    pub fn database_path(&self) -> Option<&Path> {
        self.database_path.as_deref()
    }
}

/// Acquire the canonical database owner used by both V21 boot cutover and
/// transactional blob sweep. Callers must retain the returned guard across
/// every stage that must exclude the other protocol.
pub async fn acquire_database_gc_owner(sql: &dyn SqlAccess) -> StorageResult<DatabaseGcOwnerGuard> {
    let database_path = sql.database_path();
    tokio::task::spawn_blocking(move || acquire_database_gc_owner_for_path_blocking(database_path))
        .await
        .map_err(|error| {
            StorageError::driver(StorageCapability::Blob, "acquire_database_gc_owner", error)
        })?
}

/// Process-wide registry of per-canonical-root write locks.
///
/// A `Mutex` field scoped to one `FsBlobStore` instance does NOT serialize
/// writes across independently constructed stores for the same root — and
/// callers construct fresh stores for the same root routinely
/// (`StorageBackend::blob_store` builds a new `FsBlobStore` on every call).
/// Keying a shared `Arc<tokio::sync::Mutex<()>>` by
/// the filesystem's own canonical path closes that gap: every `FsBlobStore`
/// for the same root, however many separate `new` calls produced them,
/// resolves to the exact same lock.
fn root_write_locks() -> &'static StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Look up (or create) the shared write lock for `root`'s canonical path.
///
/// `root` must already exist when this is called — `FsBlobStore::new`
/// creates it first, and `Path::canonicalize` requires the path to exist.
/// The lookup-or-insert happens under the registry's own (synchronous, very
/// briefly held) lock, so two `FsBlobStore::new` calls racing for the same
/// root cannot each install a different `Arc` and defeat the sharing this
/// exists for.
fn write_lock_for_root(root: &Path) -> std::io::Result<Arc<tokio::sync::Mutex<()>>> {
    let canonical = root.canonicalize()?;
    let mut locks = root_write_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(locks
        .entry(canonical)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}

/// A `BlobStore` backed by a BLAKE3-sharded directory tree.
#[derive(Debug)]
pub struct FsBlobStore {
    root: PathBuf,
    /// Initialization-time filesystem authority for `root`. Every blob
    /// descriptor walk starts from this retained handle rather than
    /// re-opening the root path and trusting its mutable ancestors again.
    root_handle: Arc<fs::File>,
    floor_bytes: u64,
    /// Shared per-canonical-root guard (see `write_lock_for_root`) that
    /// serializes the check-then-publish critical section of `put`: without
    /// this, two puts (whether on the same
    /// `FsBlobStore` instance or two independently constructed ones for the
    /// same root) can each observe the same pre-write `available_space`
    /// snapshot, each pass their own write-size-aware floor check against
    /// it, and then both write, jointly pushing the volume under the floor.
    /// `put` acquires this as an OWNED guard (`lock_owned`) and MOVES it
    /// into the `spawn_blocking` closure rather than borrowing it across the
    /// closure's `.await` — cancelling/dropping the outer `put` future then
    /// cannot release the guard before the underlying blocking write (which
    /// keeps running on its own thread regardless of the outer future's
    /// fate) actually finishes. A per-root async mutex is adequate at this
    /// write rate. The blocking write also takes a root-local advisory file
    /// lock to coordinate with publishers and transactional sweeps in other
    /// processes.
    write_lock: Arc<tokio::sync::Mutex<()>>,
    /// How long a blob with zero live references is left alone before an
    /// orphan sweep will delete it — see `within_publish_grace`. Bounds the
    /// window between `put` (bytes land, lock released) and the later,
    /// separate attachment write that commits a `content_ref` to it; it does not
    /// close that window entirely; see `within_publish_grace` and
    /// `transactional_orphan_sweep`'s doc comment for the residual exposure.
    orphan_sweep_grace: Duration,
}

impl FsBlobStore {
    /// Default fail-closed free-space floor (khive#292 SPEC-gate ruling):
    /// 100 GB. Config-overridable via the `floor_bytes` constructor argument.
    pub const DEFAULT_FLOOR_BYTES: u64 = 100_000_000_000;

    /// Default orphan-sweep publish grace period: 1 hour. Generous on
    /// purpose — it only needs to outlast the gap between a client's `put`
    /// call returning and its follow-up attachment write landing, not any
    /// steady-state condition.
    pub const DEFAULT_ORPHAN_SWEEP_GRACE: Duration = Duration::from_secs(3600);

    /// Create a store rooted at `root`, creating the directory if absent.
    pub fn new(root: PathBuf, floor_bytes: u64) -> Result<Self, SqliteError> {
        fs::create_dir_all(&root)?;
        Self::open_existing(root, floor_bytes)
    }

    /// Open a store rooted at an existing directory without creating any
    /// filesystem entry. Used by snapshot runtimes so boot can retain blob
    /// reads while remaining side-effect free; mutation is fenced by the
    /// runtime's read-only wrapper.
    pub fn open_existing(root: PathBuf, floor_bytes: u64) -> Result<Self, SqliteError> {
        // Preserve existing support for configured symlink spellings without
        // carrying that mutable indirection into handle-relative reads. The
        // bounded reader deliberately opens its stored root with NOFOLLOW.
        let root = root.canonicalize()?;
        let metadata = fs::metadata(&root)?;
        if !metadata.is_dir() {
            return Err(SqliteError::InvalidData(format!(
                "blob store root is not a directory: {}",
                root.display()
            )));
        }
        let root_handle = Arc::new(open_blob_root_handle(&root)?);
        let write_lock = write_lock_for_root(&root)?;
        verify_blob_root_identity(&root, &root_handle)?;
        Ok(Self {
            root,
            root_handle,
            floor_bytes,
            write_lock,
            orphan_sweep_grace: Self::DEFAULT_ORPHAN_SWEEP_GRACE,
        })
    }

    /// Override the orphan-sweep publish grace period (default: one hour —
    /// see `DEFAULT_ORPHAN_SWEEP_GRACE`).
    pub fn with_orphan_sweep_grace(mut self, grace_period: Duration) -> Self {
        self.orphan_sweep_grace = grace_period;
        self
    }

    /// The resolved root directory this store writes under.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        // OWNED guard, MOVED into the blocking closure below: a guard merely
        // borrowed here and held in this
        // async fn's own stack frame would be released the instant the
        // *outer* `put` future is cancelled or dropped, while an
        // already-started `spawn_blocking` closure keeps running on its own
        // thread regardless — letting a second `put` pass its check against
        // an unprotected in-flight write. Moving the owned guard into the
        // closure ties its lifetime to the blocking work itself, not to
        // whether anyone is still awaiting this future.
        let owned_guard = self.write_lock.clone().lock_owned().await;
        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let floor_bytes = self.floor_bytes;
        // `sync_hook::take` (added for PR #922) is the
        // test-only seam that lets regression tests observe/control
        // exactly when this call is inside the guarded section, replacing
        // fixed-sleep/fixed-duration-poll timing assumptions with
        // deterministic, event-driven synchronization. `#[cfg(test)]`-
        // gated end to end -- zero effect on non-test builds.
        #[cfg(test)]
        let hook = sync_hook::take(&root);
        tokio::task::spawn_blocking(move || {
            // The guard lives in this inner block so it is dropped BEFORE
            // the test hook's `done` signal fires below -- a test that
            // waits on `done` and then immediately asserts the lock is
            // free needs that ordering to hold exactly, not "usually".
            #[cfg_attr(not(test), allow(clippy::let_and_return))]
            let result = {
                let _owned_guard = owned_guard;
                #[cfg(test)]
                if let Some(h) = &hook {
                    let _ = h.reached.send(());
                    let _ = h.release.recv();
                }
                put_blocking_from_root_handle(&root, &root_handle, floor_bytes, bytes)
            };
            #[cfg(test)]
            if let Some(h) = &hook {
                let _ = h.done.send(());
            }
            result
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "put", e))?
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        // Argument validation is deliberately outside `spawn_blocking`: an
        // invalid portable-envelope request must fail before any backend work
        // is scheduled or the filesystem is touched (ADR-160 D2).
        if max_bytes > MAX_BLOB_WHOLE_BYTES {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Blob,
                operation: "get_bounded_verified".into(),
                message: format!(
                    "max_bytes {max_bytes} exceeds the {MAX_BLOB_WHOLE_BYTES}-byte portable whole-buffer envelope"
                ),
            });
        }

        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let content_ref = content_ref.clone();
        #[cfg(test)]
        let read_hook = bounded_read_sync_hook::take(&root);
        tokio::task::spawn_blocking(move || {
            // Open exactly once. Both metadata and bytes below come from this
            // no-follow handle, so replacing the path after open cannot
            // switch the integrity authority underneath the read.
            let mut file = open_blob_shard_file_no_follow(&root, &root_handle, &content_ref)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        StorageError::NotFound {
                            capability: StorageCapability::Blob,
                            resource: "blob",
                            key: content_ref.to_string(),
                        }
                    } else if e.kind() == std::io::ErrorKind::Unsupported {
                        StorageError::Unsupported {
                            capability: StorageCapability::Blob,
                            operation: "get_bounded_verified".into(),
                            message: e.to_string(),
                        }
                    } else {
                        map_io_err(e, "get_bounded_verified.open")
                    }
                })?;

            let metadata_bytes = file
                .metadata()
                .map_err(|e| map_io_err(e, "get_bounded_verified.metadata"))?
                .len();
            #[cfg(test)]
            if let Some(hook) = &read_hook {
                let _ = hook.reached.send(());
                let _ = hook.release.recv();
            }
            if metadata_bytes > max_bytes {
                return Err(StorageError::BlobTooLarge {
                    content_ref,
                    max_bytes,
                    observed_at_least: metadata_bytes,
                });
            }

            // Read at most one sentinel byte beyond the caller's limit. The
            // +1 is safe because max_bytes was already bounded to 64 MiB.
            let mut bytes = Vec::with_capacity(metadata_bytes as usize);
            (&mut file)
                .take(max_bytes + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| map_io_err(e, "get_bounded_verified.read"))?;
            let actual_bytes = bytes.len() as u64;
            if actual_bytes > max_bytes {
                return Err(StorageError::BlobTooLarge {
                    content_ref,
                    max_bytes,
                    observed_at_least: actual_bytes,
                });
            }
            if metadata_bytes != actual_bytes {
                return Err(StorageError::BlobSizeMismatch {
                    content_ref,
                    metadata_bytes,
                    actual_bytes,
                });
            }

            let actual = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
            if actual != content_ref {
                return Err(StorageError::BlobDigestMismatch {
                    expected: content_ref,
                    actual,
                });
            }
            Ok(bytes)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "get_bounded_verified", e))?
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let content_ref = content_ref.clone();
        tokio::task::spawn_blocking(move || {
            match open_blob_shard_file_no_follow(&root, &root_handle, &content_ref) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(map_io_err(error, "exists")),
            }
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "exists", e))?
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let content_ref = content_ref.clone();
        tokio::task::spawn_blocking(move || {
            match open_blob_shard_file_no_follow(&root, &root_handle, &content_ref) {
                Ok(file) => file
                    .metadata()
                    .map(|metadata| Some(metadata.len()))
                    .map_err(|error| map_io_err(error, "size")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(map_io_err(error, "size")),
            }
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "size", e))?
    }

    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let content_ref = content_ref.clone();
        tokio::task::spawn_blocking(move || {
            match unlink_blob_shard_file_no_follow(&root, &root_handle, &content_ref) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(map_io_err(e, "delete")),
            }
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "delete", e))?
    }

    // Disabled for this compatibility release (Phase4a, ADR-111 §8 amended
    // 2026-08-21): this API has no `SqlAccess` capability of its own, so it
    // cannot prove the completed-V21 epoch `transactional_orphan_sweep`
    // requires before any destructive path runs. A caller-assembled
    // `live_refs` snapshot could otherwise delete an object a V20 SQL query
    // cannot see as live (e.g. a moodboard FANN network), bypassing the
    // epoch gate entirely. Report-only and destructive calls both refuse,
    // matching the trait default, until a snapshot API can carry its own
    // epoch proof.
    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> {
        let _ = config;
        Err(StorageError::Unsupported {
            capability: StorageCapability::Blob,
            operation: "orphan_sweep".into(),
            message: "caller-snapshot orphan_sweep is disabled in this compatibility release; \
                      it cannot prove a completed V21 attachment epoch, use \
                      transactional_orphan_sweep instead"
                .into(),
        })
    }

    // `put` and the attachment write that later commits a `content_ref` to its
    // result are two separate steps of the client protocol -- the write
    // lock this method takes only serializes it against a concurrent `put`,
    // it is not held across the caller's own gap between finishing `put` and
    // issuing that follow-up attachment write. A blob can therefore be fully on
    // disk with zero live references purely because its referencing write
    // hasn't landed yet, not because it is actually orphaned.
    // `within_publish_grace` (via `orphan_sweep_grace`) is what protects that
    // window: a file younger than the grace period is left alone regardless
    // of liveness. Residual assumption: a client that waits longer than the
    // grace period between `put` returning and its attachment write committing
    // is still exposed to this method deleting the blob out from under it --
    // callers with an unusually slow publish path should widen the grace
    // period (`FsBlobStore::with_orphan_sweep_grace`) accordingly.
    //
    // Cross-resource ordering (#1850): database/root ownership and filesystem
    // walk/metadata happen before SQL. Bounded SQL-only units recover abandoned
    // rows and commit at most 128 fresh claims whose attachment triggers fence new
    // live references; physical deletion happens after each COMMIT; a second
    // bounded SQL-only unit releases that batch. Database/root owners span the
    // destructive phases, but SQLite's single writer never spans external I/O.
    async fn transactional_orphan_sweep(
        &self,
        sql: &dyn SqlAccess,
        dry_run: bool,
    ) -> StorageResult<BlobOrphanSweepResult> {
        // This compatibility gate deliberately precedes every database/root
        // owner wait, filesystem walk, and abandoned-claim cleanup. V20 and
        // staged V21 cannot represent the complete attachment liveness set,
        // so even report-only sweeps must refuse without observable mutation.
        if !blob_gc_fencing_complete(sql).await? {
            return Err(unsupported_blob_gc_epoch());
        }

        // Claims and their attachment triggers are database-global. Serialize the
        // whole cross-resource protocol by database before taking the root
        // locks, so differently configured roots cannot recover one another's
        // active claim batches. The OS lock is the crash-detecting owner:
        // acquiring it proves that every row left in this database is
        // abandoned, including rows copied by backup or left before a root
        // relocation.
        let database_path = sql.database_path();
        let lock_database_path = database_path.clone();
        #[cfg(test)]
        let hook_database_path = database_path.clone();
        let (database_guard, database_file_guard) = tokio::task::spawn_blocking(move || {
            let process_guard = sweep_lock_for_database(lock_database_path.as_deref()).acquire();
            let file_guard = acquire_database_gc_lock(lock_database_path.as_deref())?;
            #[cfg(test)]
            if let Some(hook) = db_ownership_sync_hook::take(hook_database_path.as_deref()) {
                let _ = hook.reached.send(());
                let _ = hook.release.recv();
            }
            Ok::<_, StorageError>((process_guard, file_guard))
        })
        .await
        .map_err(|e| {
            StorageError::driver(
                StorageCapability::Blob,
                "transactional_orphan_sweep_lock",
                e,
            )
        })??;

        // Recheck immediately once database ownership -- the process-local
        // guard plus the cross-process advisory lock -- is held, and before
        // ever waiting on the root guard/lock or walking the filesystem, so
        // the documented pre-lock refusal contract holds even if the epoch
        // regressed between the read-only preflight above and ownership.
        if !blob_gc_fencing_complete(sql).await? {
            return Err(unsupported_blob_gc_epoch());
        }

        let root_guard = self.write_lock.clone().lock_owned().await;
        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let scan_root = root.clone();
        let scan_root_handle = Arc::clone(&root_handle);
        let grace_period = self.orphan_sweep_grace;
        let (write_guards, canonical_root, prepared) = tokio::task::spawn_blocking(move || {
            verify_blob_root_identity(&scan_root, &scan_root_handle)
                .map_err(|e| map_io_err(e, "transactional_orphan_sweep_root"))?;
            // `self.root` was canonicalized at construction. Preserve that
            // spelling instead of re-resolving mutable ancestors here.
            let canonical_root = scan_root;
            let root_write_guard =
                acquire_root_write_lock_anchored(&canonical_root, &scan_root_handle)?;
            let candidates = walk_blob_files_from_root_handle(&scan_root_handle, &canonical_root)
                .map_err(|e| map_io_err(e, "transactional_orphan_sweep_walk"))?;
            verify_blob_root_identity(&canonical_root, &scan_root_handle)
                .map_err(|e| map_io_err(e, "transactional_orphan_sweep_root"))?;
            let prepared = prepare_transactional_sweep(candidates, grace_period);
            Ok::<_, StorageError>((
                (
                    database_guard,
                    database_file_guard,
                    root_guard,
                    root_write_guard,
                ),
                canonical_root,
                prepared,
            ))
        })
        .await
        .map_err(|e| {
            StorageError::driver(
                StorageCapability::Blob,
                "transactional_orphan_sweep_walk",
                e,
            )
        })??;
        let root_key = blob_root_key(&canonical_root);
        validate_blob_gc_evidence(sql).await?;
        blob_gc_fence_probe(sql).await?;
        if !dry_run {
            loop {
                let released = release_abandoned_blob_gc_claim_batch(sql).await?;
                if released < BLOB_GC_CLAIM_BATCH_SIZE as u64 {
                    break;
                }
            }
        }

        let mut write_guards = write_guards;
        let mut result = prepared.result;
        let mut delete_error = None;
        #[cfg(test)]
        let mut hook: Option<sync_hook::Hook> = None;
        #[cfg(not(test))]
        let mut hook: Option<()> = None;
        #[cfg(test)]
        let mut hook_paused = false;

        // Every unit below has a strict cardinality bound. The database owner
        // and root locks span the sequence, while each claim transaction and
        // cleanup transaction commits before filesystem work or the next
        // batch. SQLite can therefore checkpoint/reuse claim-table pages
        // between batches instead of receiving one orphan-population-sized
        // transaction.
        for candidates in prepared.candidates.chunks(BLOB_GC_CLAIM_BATCH_SIZE) {
            let batch = claim_blob_gc_batch(sql, root_key.clone(), candidates, dry_run).await?;
            result.grace_period_skipped += batch.grace_period_skipped;
            result.would_delete += batch.would_delete;
            if dry_run {
                continue;
            }

            let claimed_refs = parse_blob_gc_claim_rows(batch.claimed_rows)?;
            if claimed_refs.is_empty() {
                continue;
            }

            #[cfg(test)]
            if hook.is_none() {
                hook = sync_hook::take(&root);
            }
            #[cfg(test)]
            let pause_hook = hook.is_some() && !hook_paused;
            #[cfg(test)]
            if pause_hook {
                hook_paused = true;
            }

            let delete_root = canonical_root.clone();
            let delete_root_handle = Arc::clone(&root_handle);
            let (returned_guards, deleted, batch_delete_error, returned_hook) =
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if pause_hook {
                        if let Some(hook) = &hook {
                            let _ = hook.reached.send(());
                            let _ = hook.release.recv();
                        }
                    }

                    let mut deleted = 0_u64;
                    let mut first_error = None;
                    for content_ref in claimed_refs {
                        match unlink_blob_shard_file_no_follow(
                            &delete_root,
                            &delete_root_handle,
                            &content_ref,
                        ) {
                            Ok(()) => deleted += 1,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                first_error =
                                    Some(map_io_err(error, "transactional_orphan_sweep_delete"));
                                break;
                            }
                        }
                    }
                    // Field order is load-bearing under cancellation: a
                    // discarded blocking-task result drops tuple fields from
                    // left to right, so both owner guards release before the
                    // test hook's `done` sender disconnects.
                    (write_guards, deleted, first_error, hook)
                })
                .await
                .map_err(|error| {
                    StorageError::driver(
                        StorageCapability::Blob,
                        "transactional_orphan_sweep_delete",
                        error,
                    )
                })?;
            write_guards = returned_guards;
            hook = returned_hook;
            result.deleted += deleted;

            // Release only this bounded batch after its physical phase. On
            // cancellation or process death before this commit, the claims
            // remain fail-closed and the next exclusive database owner
            // reevaluates them rather than resuming deletion blindly.
            release_blob_gc_batch(sql, root_key.clone()).await?;
            if batch_delete_error.is_some() {
                delete_error = batch_delete_error;
                break;
            }
        }

        drop(write_guards);
        #[cfg(test)]
        if let Some(hook) = hook {
            let _ = hook.done.send(());
        }
        #[cfg(not(test))]
        let _ = hook;
        if let Some(error) = delete_error {
            return Err(error);
        }
        Ok(result)
    }
}

/// Test-only synchronization seam into blob write-lock-guarded critical
/// sections (added for PR #922 and reused by the transactional sweep).
///
/// The prior regression tests proved mutual exclusion and cancellation-
/// safety with a fixed sleep before racing/aborting and a fixed-duration
/// poll loop waiting for the lock to free -- timing-dependent, and the poll
/// loop actually failed once in a required-suite run (a flaky
/// gate, not a real regression). This seam replaces both edges of the race
/// with event-driven coordination: a one-shot hook, queued per canonical
/// root, signals `reached` the instant execution is inside the guarded
/// closure (the owned guard already moved in) and blocks there until the
/// test sends `release`; `done` fires only after the guard has actually
/// been dropped (see `put`'s inner-block scoping of `_owned_guard`).
/// `#[cfg(test)]`-gated end to end -- zero effect on non-test builds.
#[cfg(test)]
mod sync_hook {
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex as StdMutex, OnceLock};

    pub(super) struct Hook {
        pub(super) reached: Sender<()>,
        pub(super) release: Receiver<()>,
        pub(super) done: Sender<()>,
    }

    fn registry() -> &'static StdMutex<HashMap<PathBuf, VecDeque<Hook>>> {
        static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, VecDeque<Hook>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
    }

    /// Queue a one-shot hook for the next instrumented operation against
    /// `root`'s canonical path. Consumed exactly once, FIFO.
    pub(super) fn install(root: &Path) -> (Receiver<()>, Sender<()>, Receiver<()>) {
        let canonical = root
            .canonicalize()
            .expect("root must exist before installing a sync_hook");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(canonical)
            .or_default()
            .push_back(Hook {
                reached: reached_tx,
                release: release_rx,
                done: done_tx,
            });
        (reached_rx, release_tx, done_rx)
    }

    /// Pop the next queued hook for `root`'s canonical path, if any (`None`
    /// for every ordinary, non-instrumented test -- `put` runs completely
    /// unaffected). `root` need not be pre-canonicalized by the caller --
    /// both `install` and `take` canonicalize, matching how
    /// `write_lock_for_root` keys the shared lock registry.
    pub(super) fn take(root: &Path) -> Option<Hook> {
        let canonical = root.canonicalize().ok()?;
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&canonical)
            .and_then(VecDeque::pop_front)
    }
}

/// Test-only pause fired the next time the orphan-sweep walk is about to
/// open a leaf candidate file in `walk_blob_files_from_root_handle`. Lets a
/// test replace that exact on-disk leaf entry with a symlink to an outside
/// decoy between candidate discovery and the handle-relative open/fstat that
/// classifies it, proving the classification stays anchored to the opened
/// handle rather than re-resolving a path.
///
/// Keyed by the walking store's canonical root path (`entry`/`take` mirror
/// `sync_hook` above), not a single process-wide slot: a process-wide slot
/// is stealable by ANY other sweep walk that reaches a valid leaf while
/// running concurrently in the same test binary (the crate's default
/// parallel test runner interleaves `#[tokio::test]` functions), which made
/// the swap regression test flaky under that interleaving. Each test in this
/// module sweeps its own tempdir root, so keying by canonical root gives
/// each test's hook install/take pair exclusive use of its own slot.
#[cfg(test)]
mod walk_leaf_sync_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex as StdMutex, OnceLock};

    pub(super) struct Hook {
        pub(super) reached: Sender<()>,
        pub(super) release: Receiver<()>,
    }

    fn registry() -> &'static StdMutex<HashMap<PathBuf, Hook>> {
        static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Hook>>> = OnceLock::new();
        REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
    }

    pub(super) fn install(root: &Path) -> (Receiver<()>, Sender<()>) {
        let canonical = root
            .canonicalize()
            .expect("root must exist before installing a walk_leaf_sync_hook");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                canonical,
                Hook {
                    reached: reached_tx,
                    release: release_rx,
                },
            );
        (reached_rx, release_tx)
    }

    /// `root` need not be pre-canonicalized by the caller -- both `install`
    /// and `take` canonicalize.
    pub(super) fn take(root: &Path) -> Option<Hook> {
        let canonical = root.canonicalize().ok()?;
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&canonical)
    }
}

/// Test-only pause after a bounded read has opened its authoritative handle
/// and captured handle metadata, but before its first byte read. This makes
/// append/truncate/path-replacement races deterministic without timing sleeps
/// and is deliberately separate from the put/GC lock hook above.
#[cfg(test)]
mod bounded_read_sync_hook {
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex as StdMutex, OnceLock};

    pub(super) struct Hook {
        pub(super) reached: Sender<()>,
        pub(super) release: Receiver<()>,
    }

    fn registry() -> &'static StdMutex<HashMap<PathBuf, VecDeque<Hook>>> {
        static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, VecDeque<Hook>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
    }

    pub(super) fn install(root: &Path) -> (Receiver<()>, Sender<()>) {
        let canonical = root
            .canonicalize()
            .expect("root must exist before installing a bounded-read hook");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(canonical)
            .or_default()
            .push_back(Hook {
                reached: reached_tx,
                release: release_rx,
            });
        (reached_rx, release_tx)
    }

    pub(super) fn take(root: &Path) -> Option<Hook> {
        let canonical = root.canonicalize().ok()?;
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&canonical)
            .and_then(VecDeque::pop_front)
    }
}

/// Test-only pause on `transactional_orphan_sweep`'s database-ownership
/// blocking task, right after the cross-process advisory lock is acquired
/// and before the epoch recheck that immediately follows it. Lets a test
/// mutate the database strictly between the read-only preflight and the
/// recheck, making the recheck's ordering relative to the root guard/lock
/// deterministic instead of racing on scheduling.
#[cfg(test)]
mod db_ownership_sync_hook {
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Mutex as StdMutex, OnceLock};

    pub(super) struct Hook {
        pub(super) reached: Sender<()>,
        pub(super) release: Receiver<()>,
    }

    fn registry() -> &'static StdMutex<HashMap<Option<PathBuf>, VecDeque<Hook>>> {
        static REGISTRY: OnceLock<StdMutex<HashMap<Option<PathBuf>, VecDeque<Hook>>>> =
            OnceLock::new();
        REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
    }

    pub(super) fn install(database_path: Option<&Path>) -> (Receiver<()>, Sender<()>) {
        let key = database_path.map(Path::to_path_buf);
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default()
            .push_back(Hook {
                reached: reached_tx,
                release: release_rx,
            });
        (reached_rx, release_tx)
    }

    pub(super) fn take(database_path: Option<&Path>) -> Option<Hook> {
        let key = database_path.map(Path::to_path_buf);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(floor_bytes: u64) -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        // Zero orphan-sweep grace period: these tests exercise immediate
        // orphan deletion, not the publish-grace window (covered by the
        // `orphan_sweep_grace` tests below).
        let store = FsBlobStore::new(root, floor_bytes)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        (dir, store)
    }

    /// Build the exact historical V20 prefix without invoking the V21
    /// zero-reference fast path in [`crate::run_migrations`].
    fn prepare_v20_gc_fixture(conn: &mut rusqlite::Connection) {
        conn.execute_batch(include_str!("../../sql/schema-migrations-table.sql"))
            .expect("create migration ledger");
        for migration in crate::MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= 20)
        {
            let tx = conn.transaction().expect("begin historical migration");
            tx.execute_batch(migration.up)
                .expect("apply historical migration body");
            tx.execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) \
                 VALUES (?1, ?2, 0)",
                rusqlite::params![migration.version, migration.name],
            )
            .expect("record historical migration");
            tx.commit().expect("commit historical migration");
        }
    }

    /// Build the canonical completed V21 schema used by transactional-GC
    /// tests. Phase 4b owns the real cutover now, so tests exercise its schema
    /// instead of retaining Phase 4a's synthetic future-schema fixture.
    fn prepare_completed_v21_gc_fixture(conn: &mut rusqlite::Connection) {
        let version = crate::run_migrations(conn).expect("prepare canonical completed V21");
        assert_eq!(version, 21, "empty fixture must take the V21 fast path");
    }

    #[tokio::test]
    async fn completed_v21_gc_gate_requires_new_indexes_and_absent_legacy_column() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        assert!(blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap());

        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute_batch("DROP INDEX idx_attachments_content_ref")
                .unwrap();
        }
        assert!(!blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap());

        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE INDEX idx_attachments_content_ref \
                         ON attachments(content_ref); \
                     ALTER TABLE entities ADD COLUMN content_ref TEXT",
                )
                .unwrap();
        }
        assert!(!blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap());
    }

    /// Table-driven acceptance matrix: starting from a fixture that passes
    /// the gate, remove exactly one required table/index/trigger/marker/
    /// ledger fact at a time and prove the gate rejects it, no probe or
    /// abandoned-claim residue survives, and every candidate file and
    /// pre-existing claim is untouched. A wrong implementation that stops
    /// requiring any single one of these facts (e.g. the claims content_ref
    /// index, the V21 ledger row/name/max, the marker row/`completed_at`, or
    /// the INSERT fence alone) would still pass the narrower pre-existing
    /// tests but fails here.
    #[tokio::test]
    async fn completed_v21_gate_acceptance_matrix_rejects_each_removed_fact_independently() {
        let cases: &[(&str, &str)] = &[
            (
                "blob_gc_claims_content_ref_index_dropped",
                "DROP INDEX idx_blob_gc_claims_content_ref",
            ),
            (
                "v21_ledger_row_deleted",
                "DELETE FROM _schema_migrations WHERE version = 21",
            ),
            (
                "v21_ledger_row_renamed",
                "UPDATE _schema_migrations SET name = 'not_attachments_first_class' \
                 WHERE version = 21",
            ),
            (
                "ledger_max_version_advances_past_v21",
                "INSERT INTO _schema_migrations (version, name, applied_at) \
                 VALUES (22, 'future_migration', 22)",
            ),
            (
                "marker_row_deleted",
                "DELETE FROM attachment_cutover_state WHERE singleton = 1",
            ),
            (
                // The schema's own compound CHECK constraint already forbids
                // `state = 'complete' AND completed_at IS NULL` via a plain
                // UPDATE, so this rebuilds the table without that constraint
                // to construct the row directly -- proving the gate's own
                // `completed_at IS NOT NULL` predicate rejects it too,
                // independent of the CHECK constraint's own enforcement.
                "marker_completed_at_null_while_state_complete",
                "DROP TABLE attachment_cutover_state; \
                 CREATE TABLE attachment_cutover_state ( \
                     singleton    INTEGER PRIMARY KEY CHECK (singleton = 1), \
                     state        TEXT NOT NULL CHECK (state IN ('incomplete', 'complete')), \
                     started_at   INTEGER NOT NULL, \
                     completed_at INTEGER \
                 ) STRICT; \
                 INSERT INTO attachment_cutover_state \
                     (singleton, state, started_at, completed_at) \
                 VALUES (1, 'complete', 21, NULL)",
            ),
            (
                "insert_fence_dropped_alone",
                "DROP TRIGGER attachments_reject_claimed_blob_insert",
            ),
        ];

        for (case, mutation_sql) in cases {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("khive.db");
            let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
            {
                let mut writer = backend.pool().writer().unwrap();
                prepare_completed_v21_gc_fixture(writer.conn_mut());
                writer
                    .conn_mut()
                    .execute_batch(mutation_sql)
                    .unwrap_or_else(|e| panic!("case {case}: failed to apply mutation: {e}"));
            }

            assert!(
                !blob_gc_fencing_complete(backend.sql().as_ref())
                    .await
                    .unwrap(),
                "case {case}: gate must reject with this fact removed"
            );

            let store = Arc::new(
                FsBlobStore::new(dir.path().join("blobs"), 0)
                    .unwrap()
                    .with_orphan_sweep_grace(Duration::ZERO),
            );
            let orphan = store
                .put(format!("gate matrix orphan for {case}").into_bytes())
                .await
                .unwrap();
            let abandoned_ref = "c".repeat(64);
            {
                let writer = backend.pool().writer().unwrap();
                writer
                    .conn()
                    .execute(
                        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                         VALUES ('gate-matrix-abandoned', ?1, 1)",
                        [abandoned_ref.as_str()],
                    )
                    .unwrap();
            }

            let _root_guard = store.write_lock.clone().lock_owned().await;
            for dry_run in [true, false] {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(1),
                    store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
                )
                .await
                .unwrap_or_else(|_| panic!("case {case}: refusal must precede the root wait"));
                assert!(
                    matches!(outcome, Err(StorageError::Unsupported { .. })),
                    "case {case} dry_run={dry_run}: expected Unsupported, got {outcome:?}"
                );
            }

            assert!(
                store.exists(&orphan).await.unwrap(),
                "case {case}: a refused sweep must not delete anything"
            );
            let reader = backend.pool().reader().unwrap();
            let remaining: i64 = reader
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM blob_gc_claims WHERE root_key = 'gate-matrix-abandoned'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 1, "case {case}: refusal must not recover claims");

            // The gate refuses before `blob_gc_fence_probe` ever runs in
            // every one of these cases, so no probe-shaped row (the fence
            // probe's own claim/attachment id patterns) should exist in
            // either table for either dry_run mode.
            let probe_claims: i64 = reader
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM blob_gc_claims WHERE root_key GLOB '__fence_probe-*'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                probe_claims, 0,
                "case {case}: refusal must leave no fence-probe claim residue"
            );
            let probe_attachments: i64 = reader
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM attachments \
                     WHERE record_uuid GLOB '__blob-gc-fence-probe-*'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                probe_attachments, 0,
                "case {case}: refusal must leave no fence-probe attachment residue"
            );
        }
    }

    /// A read failure while evaluating the completed-marker/ledger predicate
    /// (a malformed or partially-migrated `attachment_cutover_state`) must
    /// propagate as an error, not be silently treated as "not complete" via
    /// some default-to-false path that could theoretically be confused with
    /// a permissive read elsewhere. The gate's three `query_scalar` calls all
    /// use `?`, so this pins that direction rather than leaving it assumed.
    #[tokio::test]
    async fn completed_v21_gate_fails_closed_when_marker_read_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            // `ALTER TABLE ... DROP COLUMN` refuses outright here because the
            // table's own compound CHECK constraint still references
            // `completed_at`; rebuild the table without the column instead
            // to get a genuine "no such column" read failure.
            writer
                .conn_mut()
                .execute_batch(
                    "DROP TABLE attachment_cutover_state; \
                     CREATE TABLE attachment_cutover_state ( \
                         singleton  INTEGER PRIMARY KEY CHECK (singleton = 1), \
                         state      TEXT NOT NULL CHECK (state IN ('incomplete', 'complete')), \
                         started_at INTEGER NOT NULL \
                     ) STRICT; \
                     INSERT INTO attachment_cutover_state (singleton, state, started_at) \
                     VALUES (1, 'complete', 21)",
                )
                .unwrap();
        }

        let gate_error = blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .expect_err("a marker read error must propagate, not silently resolve to false");
        assert!(
            !matches!(gate_error, StorageError::Unsupported { .. }),
            "a read error is a distinct failure from the typed epoch refusal: {gate_error:?}"
        );

        let store = Arc::new(
            FsBlobStore::new(dir.path().join("blobs"), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store
            .put(b"marker read error orphan".to_vec())
            .await
            .unwrap();
        let _root_guard = store.write_lock.clone().lock_owned().await;
        for dry_run in [true, false] {
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
            )
            .await
            .expect("a marker read error must fail before waiting on the root lock");
            assert!(
                outcome.is_err(),
                "dry_run={dry_run}: expected the sweep to fail closed, got {outcome:?}"
            );
        }
        assert!(store.exists(&orphan).await.unwrap());
    }

    #[test]
    fn database_sweep_owner_is_keyed_by_database_not_blob_root() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("khive.db");
        let same_database_a = sweep_lock_for_database(Some(&database));
        let same_database_b = sweep_lock_for_database(Some(&database));
        let other_database = sweep_lock_for_database(Some(&dir.path().join("other.db")));
        let mut expected_lock_path = database.as_os_str().to_os_string();
        expected_lock_path.push(DATABASE_GC_LOCK_SUFFIX);

        assert!(Arc::ptr_eq(&same_database_a, &same_database_b));
        assert!(!Arc::ptr_eq(&same_database_a, &other_database));
        assert_eq!(
            database_gc_lock_path(&database),
            PathBuf::from(expected_lock_path)
        );
    }

    #[tokio::test]
    async fn database_gc_owner_holds_process_and_advisory_fences_until_drop() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("owner.db");
        let backend = crate::StorageBackend::sqlite(&database).unwrap();
        let owner = acquire_database_gc_owner(backend.sql().as_ref())
            .await
            .unwrap();
        let canonical_database = owner
            .database_path()
            .expect("file-backed owner path")
            .to_path_buf();

        assert!(
            sweep_lock_for_database(Some(&canonical_database))
                .try_acquire()
                .is_none(),
            "boot and sweep must share one process-local database owner"
        );
        let external = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(database_gc_lock_path(&canonical_database))
            .unwrap();
        assert!(
            matches!(
                fs4::FileExt::try_lock(&external),
                Err(fs4::TryLockError::WouldBlock)
            ),
            "the reusable owner must also retain the cross-process advisory fence"
        );

        drop(owner);
        fs4::FileExt::try_lock(&external).expect("owner drop releases advisory fence");
    }

    #[cfg(unix)]
    #[test]
    fn database_gc_lock_path_preserves_non_utf8_identity() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let database = PathBuf::from(std::ffi::OsString::from_vec(
            b"khive-non-utf8-\xff.db".to_vec(),
        ));
        let lock_path = database_gc_lock_path(&database);
        let mut expected = database.as_os_str().as_bytes().to_vec();
        expected.extend_from_slice(DATABASE_GC_LOCK_SUFFIX.as_bytes());
        assert_eq!(lock_path.as_os_str().as_bytes(), expected);
    }

    /// A shard directory replaced by a symlink (an attacker with write access
    /// to the blob root, a misconfigured shared parent, or a race between
    /// this sweep's directory walk and its physical delete) must be refused,
    /// never followed, and an unrelated real shard must keep sweeping
    /// normally. This is the fix for the shard-directory symlink-replacement
    /// hazard: a plain `fs::remove_file(shard_path(root, content_ref))`
    /// would resolve straight through the symlink and unlink whatever file
    /// its target names.
    #[cfg(unix)]
    #[tokio::test]
    async fn unlink_blob_shard_refuses_symlinked_shard_dir_and_still_sweeps_real_shard() {
        let (dir, store) = store(0);
        let root = dir.path().join("blobs");

        // Real, non-attacked blob: written through the normal `put` path and
        // must still sweep after the fix.
        let real = store.put(b"real blob content".to_vec()).await.unwrap();

        // Attack setup: a `content_ref` whose first shard directory does not
        // exist yet is replaced by a symlink to a directory entirely outside
        // the blob root, with a file planted at the exact name a path-based
        // delete would target.
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        fs::write(&victim, b"do not delete me").unwrap();

        let real_prefix = &real.as_str()[0..2];
        let attack_prefix = if real_prefix == "aa" { "bb" } else { "aa" };
        let fake_ref = ContentRef::from_hex(format!("{attack_prefix}{}", "0".repeat(62))).unwrap();
        let fake_hex = fake_ref.as_str().to_string();

        let shard1 = root.join(attack_prefix);
        std::os::unix::fs::symlink(outside.path(), &shard1).unwrap();
        // Plant the file a naive path-based delete would actually resolve
        // to through the symlink: `<outside>/<shard2>/<full-hex>` is never
        // reached because the fix refuses at the `shard1` open itself, but
        // planting it this deep proves the refusal isn't accidental — there
        // was a real target for the naive path to have deleted.
        fs::create_dir_all(outside.path().join(&fake_hex[2..4])).unwrap();
        fs::write(
            outside.path().join(&fake_hex[2..4]).join(&fake_hex),
            b"decoy",
        )
        .unwrap();

        let root_handle = open_blob_root_handle(&root).unwrap();
        let error = unlink_blob_shard_file_no_follow(&root, &root_handle, &fake_ref).unwrap_err();
        // `O_DIRECTORY | O_NOFOLLOW` against a symlink refuses to follow it,
        // but the exact errno is platform-dependent: Linux reports `ELOOP`,
        // Darwin reports `ENOTDIR` (the symlink itself is not a directory
        // once `O_NOFOLLOW` stops it from being resolved). Either way it
        // must be an outright refusal, not a successful open/unlink.
        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ),
            "opening a symlinked shard directory must be refused, not followed; got: {error}"
        );
        assert!(
            victim.exists(),
            "the file outside the blob root must never be touched by a refused shard-dir open"
        );

        // The unrelated real shard, never touched by the attack, must still
        // unlink normally after the fix. `orphan_sweep` is disabled in this
        // compatibility release (it cannot prove a completed V21 epoch), so
        // this exercises the same underlying primitive directly rather than
        // going through that disabled API.
        unlink_blob_shard_file_no_follow(&root, &root_handle, &real).unwrap();
        assert!(!store.exists(&real).await.unwrap());
    }

    /// Block on `rx.recv()` on a dedicated thread so a `#[tokio::test]`
    /// (current-thread runtime) doesn't stall other spawned tasks while
    /// waiting on a `sync_hook` signal: the deterministic,
    /// event-driven replacement for fixed-sleep / fixed-duration-poll
    /// assertions.
    async fn recv_blocking(rx: std::sync::mpsc::Receiver<()>) -> bool {
        tokio::task::spawn_blocking(move || rx.recv().is_ok())
            .await
            .expect("recv_blocking thread panicked")
    }

    #[tokio::test]
    async fn put_bounded_get_roundtrip() {
        let (_dir, store) = store(0);
        let bytes = b"hello blob store".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        let fetched = store
            .get_bounded_verified(&content_ref, bytes.len() as u64)
            .await
            .unwrap();
        assert_eq!(fetched, bytes);
    }

    #[tokio::test]
    async fn bounded_verified_get_accepts_exact_and_portable_maximum_limits() {
        let (_dir, store) = store(0);
        let bytes = b"bounded fs blob".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();

        assert_eq!(
            store
                .get_bounded_verified(&content_ref, bytes.len() as u64)
                .await
                .unwrap(),
            bytes
        );
        assert_eq!(
            store
                .get_bounded_verified(&content_ref, MAX_BLOB_WHOLE_BYTES)
                .await
                .unwrap(),
            bytes
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_verified_get_resolves_a_configured_symlink_root_once() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("blob-target");
        fs::create_dir(&target).unwrap();
        let configured = dir.path().join("blob-configured");
        symlink(&target, &configured).unwrap();

        let store = FsBlobStore::new(configured, 0).unwrap();
        assert_eq!(store.root(), target.canonicalize().unwrap());
        let bytes = b"symlink-configured root".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        assert_eq!(
            store
                .get_bounded_verified(&content_ref, bytes.len() as u64)
                .await
                .unwrap(),
            bytes
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fs_blob_store_refuses_root_replacement_before_put() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let store = FsBlobStore::new(root.clone(), 0).unwrap();
        let original_root = dir.path().join("blobs-original");
        fs::rename(&root, &original_root).unwrap();

        let redirected_root = tempfile::tempdir().unwrap();
        symlink(redirected_root.path(), &root).unwrap();

        let bytes = b"must not land in a replaced root".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        store
            .put(bytes)
            .await
            .expect_err("a root replaced after construction must be refused");

        assert!(
            !shard_path(redirected_root.path(), &content_ref).exists(),
            "put must not publish into the tree selected by the replacement symlink"
        );
        assert!(
            !shard_path(&original_root, &content_ref).exists(),
            "a refused put must not mutate the initialization-time root either"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fs_blob_store_refuses_ancestor_replacement_for_existing_blob_operations() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let ancestor = dir.path().join("store-parent");
        let root = ancestor.join("blobs");
        let store = FsBlobStore::new(root.clone(), 0).unwrap();
        let bytes = b"same bytes in both trees".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();

        let original_ancestor = dir.path().join("store-parent-original");
        fs::rename(&ancestor, &original_ancestor).unwrap();
        let original_blob = shard_path(&original_ancestor.join("blobs"), &content_ref);

        let redirected_ancestor = tempfile::tempdir().unwrap();
        let redirected_blob = shard_path(&redirected_ancestor.path().join("blobs"), &content_ref);
        fs::create_dir_all(redirected_blob.parent().unwrap()).unwrap();
        fs::write(&redirected_blob, &bytes).unwrap();
        symlink(redirected_ancestor.path(), &ancestor).unwrap();

        store
            .get_bounded_verified(&content_ref, bytes.len() as u64)
            .await
            .expect_err("a read through a replaced root ancestor must be refused");
        store
            .exists(&content_ref)
            .await
            .expect_err("exists through a replaced root ancestor must be refused");
        store
            .size(&content_ref)
            .await
            .expect_err("size through a replaced root ancestor must be refused");
        store
            .delete(&content_ref)
            .await
            .expect_err("delete through a replaced root ancestor must be refused");

        assert!(
            original_blob.exists(),
            "refusal must preserve the initialization-time blob"
        );
        assert!(
            redirected_blob.exists(),
            "refusal must not read as authority or delete the redirected blob"
        );
    }

    #[tokio::test]
    async fn bounded_verified_get_rejects_same_size_digest_corruption() {
        let (_dir, store) = store(0);
        let expected_bytes = b"expected".to_vec();
        let actual_bytes = b"mutated!".to_vec();
        let expected = store.put(expected_bytes).await.unwrap();
        let actual = ContentRef::from_digest_bytes(blake3::hash(&actual_bytes).as_bytes());
        fs::write(shard_path(store.root(), &expected), actual_bytes).unwrap();

        let err = store.get_bounded_verified(&expected, 8).await.unwrap_err();
        assert!(matches!(
            err,
            StorageError::BlobDigestMismatch {
                expected: ref got_expected,
                actual: ref got_actual,
            } if got_expected == &expected && got_actual == &actual
        ));
    }

    #[tokio::test]
    async fn bounded_verified_get_stops_at_max_plus_one_after_file_growth() {
        let (_dir, store) = store(0);
        let store = Arc::new(store);
        let content_ref = store.put(b"abcd".to_vec()).await.unwrap();
        let path = shard_path(store.root(), &content_ref);
        let (reached, release) = bounded_read_sync_hook::install(store.root());

        let read_store = Arc::clone(&store);
        let read_ref = content_ref.clone();
        let read = tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, 4).await });
        assert!(
            recv_blocking(reached).await,
            "read must reach the metadata seam"
        );
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"efgh-poison-tail").unwrap();
        writer.flush().unwrap();
        release.send(()).unwrap();

        let err = read.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            StorageError::BlobTooLarge {
                content_ref: ref got,
                max_bytes: 4,
                observed_at_least: 5,
            } if got == &content_ref
        ));
    }

    #[tokio::test]
    async fn bounded_verified_get_reports_growth_within_limit_as_size_mismatch() {
        let (_dir, store) = store(0);
        let store = Arc::new(store);
        let content_ref = store.put(b"abcd".to_vec()).await.unwrap();
        let path = shard_path(store.root(), &content_ref);
        let (reached, release) = bounded_read_sync_hook::install(store.root());

        let read_store = Arc::clone(&store);
        let read_ref = content_ref.clone();
        let read = tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, 8).await });
        assert!(
            recv_blocking(reached).await,
            "read must reach the metadata seam"
        );
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"ef").unwrap();
        writer.flush().unwrap();
        release.send(()).unwrap();

        let err = read.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            StorageError::BlobSizeMismatch {
                content_ref: ref got,
                metadata_bytes: 4,
                actual_bytes: 6,
            } if got == &content_ref
        ));
    }

    #[tokio::test]
    async fn bounded_verified_get_reports_truncation_as_size_mismatch() {
        let (_dir, store) = store(0);
        let store = Arc::new(store);
        let content_ref = store.put(b"abcd".to_vec()).await.unwrap();
        let path = shard_path(store.root(), &content_ref);
        let (reached, release) = bounded_read_sync_hook::install(store.root());

        let read_store = Arc::clone(&store);
        let read_ref = content_ref.clone();
        let read = tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, 4).await });
        assert!(
            recv_blocking(reached).await,
            "read must reach the metadata seam"
        );
        let mut writer = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        writer.write_all(b"abc").unwrap();
        writer.flush().unwrap();
        release.send(()).unwrap();

        let err = read.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            StorageError::BlobSizeMismatch {
                content_ref: ref got,
                metadata_bytes: 4,
                actual_bytes: 3,
            } if got == &content_ref
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_verified_get_keeps_the_opened_inode_when_the_path_is_replaced() {
        let (_dir, store) = store(0);
        let store = Arc::new(store);
        let original = b"original".to_vec();
        let replacement = b"replaced".to_vec();
        let content_ref = store.put(original.clone()).await.unwrap();
        let path = shard_path(store.root(), &content_ref);
        let moved_path = path.with_extension("opened-inode");
        let max_bytes = original.len() as u64;
        let (reached, release) = bounded_read_sync_hook::install(store.root());

        let read_store = Arc::clone(&store);
        let read_ref = content_ref.clone();
        let read =
            tokio::spawn(
                async move { read_store.get_bounded_verified(&read_ref, max_bytes).await },
            );
        assert!(
            recv_blocking(reached).await,
            "read must reach the metadata seam"
        );
        fs::rename(&path, &moved_path).unwrap();
        fs::write(&path, replacement).unwrap();
        release.send(()).unwrap();

        assert_eq!(read.await.unwrap().unwrap(), original);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_verified_get_refuses_a_symlink_leaf() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
        let outside_bytes = b"outside but digest matching".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&outside_bytes).as_bytes());
        let outside = dir.path().join("outside");
        fs::write(&outside, &outside_bytes).unwrap();
        let leaf = shard_path(store.root(), &content_ref);
        fs::create_dir_all(leaf.parent().unwrap()).unwrap();
        symlink(&outside, &leaf).unwrap();

        let err = store
            .get_bounded_verified(&content_ref, outside_bytes.len() as u64)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_verified_get_refuses_a_symlinked_shard_component() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
        let outside_bytes = b"outside through shard link".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&outside_bytes).as_bytes());
        let hex = content_ref.as_str();
        let outside_shard1 = dir.path().join("outside-shard1");
        let outside_shard2 = outside_shard1.join(&hex[2..4]);
        fs::create_dir_all(&outside_shard2).unwrap();
        fs::write(outside_shard2.join(hex), &outside_bytes).unwrap();
        symlink(&outside_shard1, store.root().join(&hex[0..2])).unwrap();

        let err = store
            .get_bounded_verified(&content_ref, outside_bytes.len() as u64)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn put_content_ref_matches_blake3_digest() {
        let (_dir, store) = store(0);
        let bytes = b"digest check".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        let expected = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        assert_eq!(content_ref, expected);
    }

    #[tokio::test]
    async fn put_dedups_identical_content() {
        let (_dir, store) = store(0);
        let bytes = b"same bytes twice".to_vec();
        let first = store.put(bytes.clone()).await.unwrap();
        let second = store.put(bytes.clone()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(
            store
                .get_bounded_verified(&first, bytes.len() as u64)
                .await
                .unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn exists_reflects_put_and_delete() {
        let (_dir, store) = store(0);
        let bytes = b"exists check".to_vec();
        let content_ref = store.put(bytes).await.unwrap();
        assert!(store.exists(&content_ref).await.unwrap());

        assert!(store.delete(&content_ref).await.unwrap());
        assert!(!store.exists(&content_ref).await.unwrap());
    }

    #[tokio::test]
    async fn delete_missing_content_ref_returns_false() {
        let (_dir, store) = store(0);
        let missing = ContentRef::from_hex("f".repeat(64)).unwrap();
        assert!(!store.delete(&missing).await.unwrap());
    }

    #[tokio::test]
    async fn size_reports_byte_length_for_a_present_object() {
        let (_dir, store) = store(0);
        let bytes = b"size check".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        assert_eq!(
            store.size(&content_ref).await.unwrap(),
            Some(bytes.len() as u64)
        );
    }

    #[tokio::test]
    async fn size_returns_none_for_an_absent_object() {
        let (_dir, store) = store(0);
        let missing = ContentRef::from_hex("9".repeat(64)).unwrap();
        assert_eq!(store.size(&missing).await.unwrap(), None);
    }

    #[tokio::test]
    async fn bounded_get_missing_content_ref_returns_not_found() {
        let (_dir, store) = store(0);
        let missing = ContentRef::from_hex("e".repeat(64)).unwrap();
        let err = store
            .get_bounded_verified(&missing, MAX_BLOB_WHOLE_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }));
    }

    #[tokio::test]
    async fn put_refuses_below_free_space_floor() {
        // A floor no real disk clears -> put must fail closed, not silently
        // degrade or spill elsewhere (khive#292 SPEC-gate ruling).
        let (_dir, store) = store(u64::MAX);
        let err = store.put(b"too big a floor".to_vec()).await.unwrap_err();
        match err {
            StorageError::CapacityFloor {
                floor_bytes,
                available_bytes,
                ..
            } => {
                assert_eq!(floor_bytes, u64::MAX);
                assert!(available_bytes < u64::MAX);
            }
            other => panic!("expected CapacityFloor, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn capacity_floor_error_names_the_floor_and_volume() {
        let (_dir, store) = store(u64::MAX);
        let err = store.put(b"x".to_vec()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&u64::MAX.to_string()),
            "must name the floor: {msg}"
        );
        assert!(msg.contains("Blob"), "must name the capability: {msg}");
    }

    #[test]
    fn crosses_floor_is_write_size_aware_at_the_exact_boundary() {
        // Exact-boundary case, verbatim from the report: `available ==
        // floor_bytes + 1` must still refuse a 2-byte write. A floor-only
        // check (`available < floor_bytes`) would NOT catch this — 101 is
        // not below 100 — but the write's own size must be subtracted first.
        assert!(crosses_floor(101, 2, 100));
        assert!(!crosses_floor(101, 1, 100));
    }

    #[test]
    fn crosses_floor_accepts_a_write_that_lands_exactly_on_the_floor() {
        assert!(!crosses_floor(100, 0, 100));
    }

    #[test]
    fn crosses_floor_rejects_a_write_that_lands_one_byte_under_the_floor() {
        assert!(crosses_floor(100, 1, 100));
    }

    #[test]
    fn crosses_floor_saturates_instead_of_underflowing_when_write_exceeds_available() {
        assert!(crosses_floor(10, 100, 50));
        // floor_bytes == 0 means "no floor enforced" (the convention every
        // other test in this file uses via `store(0)`) — even a write far
        // exceeding available space is not refused by the floor check itself
        // in that case; `saturating_sub` floors the subtraction at 0, and
        // `0 < 0` is false.
        assert!(!crosses_floor(10, 100, 0));
    }

    #[test]
    fn put_refuses_a_write_that_would_cross_the_floor_even_though_available_alone_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir_all(&root).unwrap();

        // Use the same blocking path as `FsBlobStore::put`, with a fixed
        // capacity snapshot: 101 bytes clears a 100-byte floor by itself,
        // but a pending two-byte write would leave only 99 bytes. Sampling
        // the host-wide APFS free-space gauge here made the old test flaky:
        // unrelated cleanup could legitimately replenish more than its
        // 64 MiB cushion between the test's sample and the put's sample.
        let err = put_blocking_with_space_probe(&root, 100, vec![7u8; 2], |_| Ok(101)).unwrap_err();
        assert!(
            matches!(err, StorageError::CapacityFloor { .. }),
            "a write-size-aware floor check must reject a write that pushes the volume \
             below the floor even though available space alone still clears it: {err:?}"
        );
    }

    #[test]
    fn a_later_put_checks_a_fresh_capacity_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir_all(&root).unwrap();

        // Model the two snapshots observed by serialized puts without tying
        // the assertion to a host-wide free-space gauge. The first two-byte
        // write may land exactly on the 100-byte floor from a 102-byte
        // snapshot. A later, different write sees 101 bytes and must refuse.
        // Mutual exclusion itself is covered deterministically below by the
        // shared-root lock test; together the tests prove the stale-snapshot
        // race is closed without relying on unrelated filesystem activity.
        let first = put_blocking_with_space_probe(&root, 100, vec![1u8; 2], |_| Ok(102));
        let second = put_blocking_with_space_probe(&root, 100, vec![2u8; 2], |_| Ok(101));

        assert!(
            first.is_ok(),
            "the first put may land on the floor: {first:?}"
        );
        assert!(
            matches!(second, Err(StorageError::CapacityFloor { .. })),
            "the later put must use its lower capacity snapshot: {second:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_puts_from_two_independently_constructed_stores_share_the_root_lock() {
        // The actual gap in the prior fix: the
        // test above uses ONE `FsBlobStore` behind a shared `Arc`, so it
        // exercises only the per-instance mutex and cannot catch a missing
        // cross-instance guarantee. `StorageBackend::blob_store` constructs
        // a FRESH `FsBlobStore` on every call, even for the same root -- so
        // the real regression is two SEPARATELY CONSTRUCTED stores for the
        // same root. Before the shared canonical-root registry, each
        // store's `write_lock` was its own independent `Mutex`, and this
        // exact scenario would have let both puts pass the same free-space
        // snapshot.
        //
        // The earlier version of this test let
        // two real `tokio::spawn`ed puts race with no control over
        // interleaving -- it could PASS on the prior per-instance-mutex
        // bug purely because the blocking thread pool happened to run them
        // sequentially, which is not a deterministic regression guard.
        //
        // The first `sync_hook`-driven attempt kept proving exclusion
        // INDIRECTLY, through a free-space floor sized to admit exactly one
        // `payload_len` write -- but this dev box's real
        // `fs4::available_space` swings by many tens to hundreds of MB in
        // either direction over the several-second window the hook
        // orchestration takes (concurrent fleet `cargo clean`/build
        // activity), and no floor margin proved robust: it was observed to
        // both under-shoot (store_a's own write refused; available_bytes
        // 25521500160 vs floor_bytes 25517096960, a ~60 MiB drop) and
        // over-shoot (store_b's write unexpectedly SUCCEEDED after
        // store_a's landed) in back-to-back runs.
        //
        // Lock sharing is orthogonal to floor arithmetic -- the same
        // `crosses_floor`/`put_blocking` path runs regardless of which
        // `FsBlobStore` instance calls it, and that arithmetic is already
        // covered deterministically by `a_later_put_checks_a_fresh_capacity_snapshot`
        // and the pure `crosses_floor` unit tests above.
        //
        // The prior fix's negative proof (B
        // must not reach its own checkpoint) still leaned on a 200ms
        // `recv_timeout` as the CORRECTNESS decision -- under sufficiently
        // delayed scheduling, old per-instance-mutex code's B could simply
        // arrive after the window and every assertion would still pass,
        // silently defeating the regression guard. Fix: assert directly
        // and immediately (no timeout, no second hook, no second
        // `tokio::spawn` racing at all) that `store_b.write_lock` -- a
        // private field, reachable here because `tests` is a child module
        // of the module that declares it -- is ALREADY held the instant
        // store_a's put holds ITS guard. Under the fixed canonical-root
        // registry this is the exact same `Arc` store_a's own `write_lock`
        // resolves to, so `try_lock()` fails with zero timing dependence;
        // under the old per-instance-mutex code, `store_b.write_lock` is a
        // completely independent, unheld `Mutex`, so `try_lock()` would
        // succeed immediately, pinning the defect on the spot regardless
        // of scheduling.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir_all(&root).unwrap();
        let canonical_root = root.canonicalize().unwrap();

        // Two INDEPENDENT `FsBlobStore::new` calls for the identical root --
        // exactly what `StorageBackend::blob_store` does on repeat calls.
        let store_a = std::sync::Arc::new(FsBlobStore::new(root.clone(), 0).unwrap());
        let store_b = std::sync::Arc::new(FsBlobStore::new(root, 0).unwrap());

        let (a_reached, a_release, _a_done) = sync_hook::install(&canonical_root);
        let a = {
            let store_a = store_a.clone();
            tokio::spawn(async move { store_a.put(b"store_a payload".to_vec()).await })
        };
        assert!(
            recv_blocking(a_reached).await,
            "store_a's put must reach the sync_hook checkpoint"
        );

        // The deterministic proof: store_b's OWN write_lock field must
        // already be unavailable while store_a holds its guard -- true
        // only if the two independently constructed stores share one
        // Arc<Mutex<()>>. No timeout, no scheduling dependence.
        assert!(
            store_b.write_lock.try_lock().is_err(),
            "store_b's write_lock was NOT held while store_a's put held its guard -- the two \
             independently constructed stores do NOT share one lock"
        );

        // Release A and let it finish. Awaiting A's outer task
        // deterministically waits for the guard to be dropped too (see
        // `put`'s inner-block scoping).
        a_release.send(()).unwrap();
        let result_a = a.await.unwrap();
        assert!(result_a.is_ok(), "store_a's put must succeed: {result_a:?}");

        // Liveness coverage: an ordinary put on store_b succeeds once
        // store_a has released the (shared) lock.
        let result_b = store_b.put(b"store_b payload".to_vec()).await;
        assert!(result_b.is_ok(), "store_b's put must succeed: {result_b:?}");
    }

    #[tokio::test]
    async fn aborting_the_outer_put_future_does_not_release_the_guard_before_persist_completes() {
        // The prior fix held the write guard only
        // in `put`'s own async stack frame (`let _write_guard = ...
        // .lock().await`) while the `spawn_blocking` closure captured just
        // root/floor_bytes/bytes. Cancelling/dropping the outer `put`
        // future released that borrowed guard immediately, even though an
        // already-started blocking write kept running on its own thread --
        // a second put could then pass its floor check while the first
        // write was still landing.
        //
        // The earlier version of this test
        // proved the fix with a fixed 10ms sleep before abort and a fixed
        // 500x10ms poll loop waiting for the lock to free -- and the poll
        // loop actually FAILED once in a required-suite run (a
        // flaky gate, not a regression). This version uses the `sync_hook`
        // seam instead: `reached` fires only once execution is genuinely
        // inside the guarded closure (owned guard already moved in) and
        // blocks there until released; `done` fires only after the guard
        // has actually been dropped (see `put`'s inner-block scoping) --
        // both edges event-driven, no sleeps, no polling.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir_all(&root).unwrap();
        let canonical_root = root.canonicalize().unwrap();

        let store = std::sync::Arc::new(FsBlobStore::new(root, 0).unwrap());
        let (reached, release, done) = sync_hook::install(&canonical_root);
        let handle = {
            let store = store.clone();
            tokio::spawn(async move { store.put(b"cancellation race payload".to_vec()).await })
        };

        assert!(
            recv_blocking(reached).await,
            "put must reach the sync_hook checkpoint -- owned guard already moved into the \
             closure -- before this test can mean anything"
        );

        handle.abort();
        let abort_result = handle.await;
        match &abort_result {
            Err(e) if e.is_cancelled() => {}
            other => panic!(
                "the outer task must actually have been cancelled for this test to be \
                 meaningful: {other:?}"
            ),
        }

        let shared_lock = write_lock_for_root(&canonical_root).unwrap();
        assert!(
            shared_lock.try_lock().is_err(),
            "the guard must still be held by the detached blocking write immediately after \
             the outer future was cancelled -- if this is free, the guard was released with \
             the aborted frame instead of moving into the spawn_blocking closure"
        );

        // Let the detached write proceed and finish, then wait for its
        // explicit completion signal -- no polling, no fixed durations.
        // `done` only fires after the guard is actually dropped (see
        // `put`), so the very next check is race-free.
        release.send(()).unwrap();
        assert!(
            recv_blocking(done).await,
            "the detached write must signal completion once it actually persists"
        );
        assert!(
            shared_lock.try_lock().is_ok(),
            "the guard must be free once the detached write's completion was observed"
        );
    }

    /// `orphan_sweep` is disabled in this compatibility release: it has no
    /// `SqlAccess` capability with which to prove a completed V21 epoch, so
    /// every call refuses regardless of `live_refs` contents or `dry_run`,
    /// and nothing on disk is ever touched. This replaces the prior
    /// `orphan_sweep_race_demonstrates_the_documented_quiescence_requirement`
    /// regression, which pinned the now-eliminated caller-snapshot deletion
    /// hazard this disablement closes.
    #[tokio::test]
    async fn orphan_sweep_is_disabled_in_both_modes_regardless_of_live_refs() {
        let (_dir, store) = store(0);
        let blob = store
            .put(b"never swept by this API".to_vec())
            .await
            .unwrap();
        let mut live_refs = std::collections::HashSet::new();
        live_refs.insert(blob.clone());

        for dry_run in [true, false] {
            let error = store
                .orphan_sweep(&BlobOrphanSweepConfig {
                    live_refs: live_refs.clone(),
                    dry_run,
                })
                .await
                .expect_err("caller-snapshot orphan_sweep must be disabled");
            assert!(
                matches!(error, StorageError::Unsupported { .. }),
                "expected typed Unsupported, got {error:?}"
            );
        }
        assert!(store.exists(&blob).await.unwrap());
    }

    /// Rollout compatibility fence: the Phase-3 binary's V20 schema cannot
    /// represent a moodboard model's nested FANN network as SQL liveness.
    /// Both report-only and destructive transactional sweeps must therefore
    /// refuse before taking the root lock or mutating abandoned claims.
    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_v20_before_root_or_claim_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_v20_gc_fixture(writer.conn_mut());
        }

        let root = dir.path().join("blobs");
        let store = Arc::new(
            FsBlobStore::new(root, 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let bundle = store.put(b"legacy model bundle".to_vec()).await.unwrap();
        let network = store.put(b"legacy FANN network".to_vec()).await.unwrap();
        let orphan = store.put(b"ordinary old orphan".to_vec()).await.unwrap();
        let abandoned_ref = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, entity_type, name, tags, created_at, updated_at, \
                      content_ref) \
                     VALUES ('legacy-model', 'local', 'artifact', 'moodboard_model', \
                             'legacy model', '[]', 1, 1, ?1)",
                    [bundle.as_str()],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-before-compat', ?1, 1)",
                    [abandoned_ref],
                )
                .unwrap();
        }

        // If the compatibility gate is below the root lock, the call parks
        // here and the timeout fails. A correct V20 refusal never waits for it.
        let _root_guard = store.write_lock.clone().lock_owned().await;
        for dry_run in [true, false] {
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
            )
            .await
            .expect("V20 refusal must happen before waiting for the held root lock");
            let error = outcome.expect_err("V20 transactional sweep must be disabled");
            match error {
                StorageError::Unsupported {
                    capability: StorageCapability::Blob,
                    operation,
                    message,
                } => {
                    assert_eq!(operation, "transactional_orphan_sweep");
                    assert!(
                        message.contains("complete V21 attachment cutover"),
                        "unexpected compatibility diagnostic: {message}"
                    );
                }
                other => panic!("expected typed Unsupported refusal, got {other:?}"),
            }
        }

        let reader = backend.pool().reader().unwrap();
        let abandoned: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = 'abandoned-before-compat' AND content_ref = ?1",
                [abandoned_ref],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(abandoned, 1, "V20 refusal must not clean abandoned claims");
        drop(reader);
        assert!(store.exists(&bundle).await.unwrap());
        assert!(store.exists(&network).await.unwrap());
        assert!(store.exists(&orphan).await.unwrap());
    }

    /// The durable marker is authoritative, not the mere presence of V21
    /// tables, triggers, or even a ledger row. An interrupted/inconsistent
    /// cutover remains non-sweepable for both modes and is rejected before
    /// the root wait or abandoned-claim recovery.
    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_incomplete_v21_marker_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        let abandoned_ref = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute(
                    "UPDATE attachment_cutover_state \
                     SET state = 'incomplete', completed_at = NULL \
                     WHERE singleton = 1",
                    [],
                )
                .unwrap();
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-incomplete-v21', ?1, 1)",
                    [abandoned_ref],
                )
                .unwrap();
        }

        let store = Arc::new(
            FsBlobStore::new(dir.path().join("blobs"), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store.put(b"incomplete V21 orphan".to_vec()).await.unwrap();
        let _root_guard = store.write_lock.clone().lock_owned().await;

        for dry_run in [true, false] {
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
            )
            .await
            .expect("incomplete V21 must refuse before waiting for the root lock");
            assert!(
                matches!(outcome, Err(StorageError::Unsupported { .. })),
                "incomplete V21 must return typed Unsupported: {outcome:?}"
            );
        }

        let remaining: i64 = backend
            .pool()
            .reader()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = 'abandoned-incomplete-v21' AND content_ref = ?1",
                [abandoned_ref],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "refusal must not recover abandoned claims");
        assert!(store.exists(&orphan).await.unwrap());
    }

    /// Both public sweep APIs must refuse under the same non-completed
    /// epochs, in both `dry_run` modes, without touching files or claims.
    /// `orphan_sweep` ignores the fixture's `SqlAccess` entirely (it has no
    /// epoch capability of its own -- that is exactly why it is disabled),
    /// so this proves the disablement holds even when a caller has a real,
    /// otherwise-plausible database sitting right next to it.
    #[tokio::test]
    async fn both_sweep_apis_refuse_v20_and_incomplete_v21_epochs_in_both_modes() {
        for incomplete_v21 in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("khive.db");
            let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
            {
                let mut writer = backend.pool().writer().unwrap();
                if incomplete_v21 {
                    prepare_completed_v21_gc_fixture(writer.conn_mut());
                    writer
                        .conn_mut()
                        .execute(
                            "UPDATE attachment_cutover_state \
                             SET state = 'incomplete', completed_at = NULL \
                             WHERE singleton = 1",
                            [],
                        )
                        .unwrap();
                } else {
                    prepare_v20_gc_fixture(writer.conn_mut());
                }
            }

            let store = FsBlobStore::new(dir.path().join("blobs"), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO);
            let orphan = store
                .put(format!("both-apis orphan (incomplete_v21={incomplete_v21})").into_bytes())
                .await
                .unwrap();

            // A known claim, seeded once per epoch case, must survive every
            // refused arm below untouched -- refusal must never recover or
            // otherwise mutate an existing claim.
            let known_claim_ref = "f".repeat(64);
            {
                let writer = backend.pool().writer().unwrap();
                writer
                    .conn()
                    .execute(
                        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                         VALUES ('both-api-known-claim', ?1, 1)",
                        [known_claim_ref.as_str()],
                    )
                    .unwrap();
            }
            let assert_known_claim_unchanged = |arm: &str| {
                let remaining: i64 = backend
                    .pool()
                    .reader()
                    .unwrap()
                    .conn()
                    .query_row(
                        "SELECT COUNT(*) FROM blob_gc_claims \
                         WHERE root_key = 'both-api-known-claim' AND content_ref = ?1",
                        [known_claim_ref.as_str()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    remaining, 1,
                    "incomplete_v21={incomplete_v21} arm={arm}: refusal must not mutate \
                     an existing claim"
                );
            };

            for dry_run in [true, false] {
                let snapshot_error = store
                    .orphan_sweep(&BlobOrphanSweepConfig {
                        live_refs: std::collections::HashSet::new(),
                        dry_run,
                    })
                    .await
                    .expect_err("orphan_sweep must refuse regardless of epoch");
                assert!(
                    matches!(snapshot_error, StorageError::Unsupported { .. }),
                    "incomplete_v21={incomplete_v21} dry_run={dry_run}: expected Unsupported \
                     from orphan_sweep, got {snapshot_error:?}"
                );
                assert_known_claim_unchanged(&format!("orphan_sweep dry_run={dry_run}"));

                let transactional_error = store
                    .transactional_orphan_sweep(backend.sql().as_ref(), dry_run)
                    .await
                    .expect_err("transactional_orphan_sweep must refuse this epoch");
                assert!(
                    matches!(transactional_error, StorageError::Unsupported { .. }),
                    "incomplete_v21={incomplete_v21} dry_run={dry_run}: expected Unsupported \
                     from transactional_orphan_sweep, got {transactional_error:?}"
                );
                assert_known_claim_unchanged(&format!(
                    "transactional_orphan_sweep dry_run={dry_run}"
                ));
            }

            assert!(
                store.exists(&orphan).await.unwrap(),
                "incomplete_v21={incomplete_v21}: a refused sweep must not delete anything"
            );
        }
    }

    /// The epoch recheck taken under database ownership must run before the
    /// sweep ever waits on the root guard/lock, even when the epoch was
    /// still valid at the read-only preflight and only regressed while the
    /// call was waiting for database ownership. Without the fix this recheck
    /// used to run after the root guard, filesystem root lock, and directory
    /// walk -- this pins the corrected ordering with a real cross-task race
    /// instead of trusting the comment above it.
    ///
    /// The externally held blocking point is the OS-level root advisory lock
    /// (`acquire_root_write_lock`), not the process-local `write_lock`
    /// mutex. The old (pre-fix) ordering acquired the process-local mutex
    /// before ever reaching database ownership -- holding that mutex here
    /// would have starved the sweep task before it ever reached the hook
    /// below, hanging this test instead of failing it. The OS-level root
    /// lock is only ever acquired after database ownership under both the
    /// old and the new ordering, so both orderings reach the hook; only the
    /// old ordering then blocks trying to acquire the lock held here,
    /// because its (mis-placed) recheck runs after that acquisition instead
    /// of before it.
    #[tokio::test]
    async fn transactional_orphan_sweep_recheck_refuses_before_root_lock_when_epoch_regresses_after_db_ownership(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        assert!(blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap());

        let blob_root = dir.path().join("blobs");
        let store = Arc::new(
            FsBlobStore::new(blob_root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store
            .put(b"epoch regressed after database ownership".to_vec())
            .await
            .unwrap();

        // Hold the same OS-level root lock the real sweep acquires, on the
        // same canonicalized path it canonicalizes to.
        let canonical_root = blob_root.canonicalize().unwrap();
        let _root_write_guard = acquire_root_write_lock(&canonical_root).unwrap();

        // Key the hook by the same canonicalized path `SqlAccess::database_path`
        // returns -- not the raw `db_path` above -- since that's what the
        // implementation looks the hook up by.
        let canonical_db_path = backend.sql().database_path();
        let (reached, release) = db_ownership_sync_hook::install(canonical_db_path.as_deref());
        let sweep_store = store.clone();
        let sweep_backend = backend.clone();
        let handle = tokio::spawn(async move {
            sweep_store
                .transactional_orphan_sweep(sweep_backend.sql().as_ref(), false)
                .await
        });

        // The hook fires only once database ownership (process-local guard
        // plus the cross-process advisory lock) is held, strictly after the
        // read-only preflight already observed a valid epoch. Bounded so a
        // regression that drops the hook call (or blocks ahead of it) fails
        // this test instead of hanging it.
        let reached_signal = tokio::time::timeout(Duration::from_secs(1), recv_blocking(reached))
            .await
            .expect("the sweep must reach database ownership before this test's timeout");
        assert!(reached_signal, "hook sender was dropped before signaling");
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute("DELETE FROM _schema_migrations WHERE version = 21", [])
                .unwrap();
        }
        release.send(()).unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect(
                "the recheck must refuse before ever waiting on the externally held root lock -- \
                 under the old (pre-fix) ordering this join times out instead, because the \
                 sweep blocks acquiring the OS-level root lock held above",
            )
            .unwrap();
        assert!(
            matches!(outcome, Err(StorageError::Unsupported { .. })),
            "expected the regressed epoch to be caught immediately after database ownership: \
             {outcome:?}"
        );
        assert!(store.exists(&orphan).await.unwrap());
    }

    /// A Phase-4a binary can remain in a mixed fleet after a newer binary has
    /// atomically completed V21. It must then use every attachment role as
    /// liveness, including a moodboard FANN network, and delete only the true
    /// orphan.
    #[tokio::test]
    async fn transactional_orphan_sweep_accepts_completed_v21_attachment_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }

        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        let bundle = store.put(b"V21 model bundle".to_vec()).await.unwrap();
        let network = store.put(b"V21 FANN network".to_vec()).await.unwrap();
        let orphan = store.put(b"V21 true orphan".to_vec()).await.unwrap();
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, entity_type, name, tags, created_at, updated_at) \
                     VALUES ('model', 'local', 'artifact', 'moodboard_model', \
                             'model', '[]', 1, 1)",
                    [],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('model', 'entity', 'content', ?1, 1), \
                            ('model', 'entity', 'fann-network', ?2, 1)",
                    rusqlite::params![bundle.as_str(), network.as_str()],
                )
                .unwrap();
        }

        let dry_run = store
            .transactional_orphan_sweep(backend.sql().as_ref(), true)
            .await
            .expect("completed V21 dry run must be supported");
        assert_eq!(dry_run.would_delete, 1);
        assert_eq!(dry_run.deleted, 0);

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .expect("completed V21 destructive sweep must be supported");
        assert_eq!(result.deleted, 1);
        assert!(store.exists(&bundle).await.unwrap());
        assert!(store.exists(&network).await.unwrap());
        assert!(!store.exists(&orphan).await.unwrap());
    }

    /// A `StorageBackend` constructed directly and never run through the
    /// versioned migration ledger (`run_migrations`/`prepare_core_schema`) —
    /// only the ad hoc, idempotent `entities` DDL a plain `entities()` call
    /// applies — has no completed V21 marker, attachment liveness table, or
    /// attachment fencing triggers. Without that set a reference committed between liveness
    /// selection and physical deletion would dangle, so the trait contract
    /// requires `StorageError::Unsupported` here rather than an unfenced
    /// sweep, and every candidate must survive.
    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_without_the_blob_gc_claims_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        backend.entities().unwrap();
        {
            let reader = backend.pool().reader().unwrap();
            let present: bool = reader
                .conn()
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' \
                     AND name = 'blob_gc_claims'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                !present,
                "this test's premise requires blob_gc_claims to be absent"
            );
        }

        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store.put(b"direct-backend orphan".to_vec()).await.unwrap();

        let sql = backend.sql();
        let error = store
            .transactional_orphan_sweep(sql.as_ref(), false)
            .await
            .expect_err("sweep must refuse a backend without the blob_gc_claims fencing set");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {error:?}"
        );
        assert!(
            store.exists(&orphan).await.unwrap(),
            "a refused sweep must not have deleted anything"
        );
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_an_incomplete_cutover_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute_batch(
                    "UPDATE attachment_cutover_state \
                     SET state = 'incomplete', completed_at = NULL WHERE singleton = 1; \
                     DELETE FROM _schema_migrations WHERE version = 21;",
                )
                .unwrap();
        }

        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        let orphan = store
            .put(b"incomplete-cutover orphan".to_vec())
            .await
            .unwrap();
        let error = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .expect_err("sweep must refuse every durable incomplete marker");
        assert!(matches!(error, StorageError::Unsupported { .. }));
        assert!(
            store.exists(&orphan).await.unwrap(),
            "refused incomplete-state sweep must preserve every blob"
        );
    }

    /// The fencing gate must demand the complete V21 set, not just the
    /// claims table: with a fencing trigger dropped, a claim no longer
    /// blocks a concurrent attachment write from resurrecting the digest, so
    /// the sweep must refuse exactly as it does with no migration at all.
    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_with_incomplete_fencing_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute_batch("DROP TRIGGER attachments_reject_claimed_blob_update")
                .unwrap();
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-partial-fence', \
                             'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd', \
                             1)",
                    [],
                )
                .unwrap();
        }

        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store.put(b"partial-fence orphan".to_vec()).await.unwrap();

        let sql = backend.sql();
        let _root_guard = store.write_lock.clone().lock_owned().await;
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            store.transactional_orphan_sweep(sql.as_ref(), false),
        )
        .await
        .expect("an incomplete V21 fence must refuse before the root wait")
        .expect_err("sweep must refuse when any V21 fencing trigger is missing");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {error:?}"
        );
        assert!(
            store.exists(&orphan).await.unwrap(),
            "a refused sweep must not have deleted anything"
        );
        let remaining: i64 = backend
            .pool()
            .reader()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = 'abandoned-partial-fence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "a refused sweep must not recover claims");
    }

    /// The gate must verify the fence FUNCTIONS, not that three names exist
    /// in `sqlite_master`: triggers with the right names but no-op bodies
    /// pass any name census while letting a claimed `content_ref` become
    /// live during the released-writer deletion window. The fence probe must
    /// catch them and refuse, deleting nothing and leaving no probe residue.
    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_same_named_noop_fencing_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute_batch(
                    "DROP TRIGGER attachments_reject_claimed_blob_insert; \
                     DROP TRIGGER attachments_reject_claimed_blob_update; \
                     CREATE TRIGGER attachments_reject_claimed_blob_insert \
                     BEFORE INSERT ON attachments BEGIN SELECT 0; END; \
                     CREATE TRIGGER attachments_reject_claimed_blob_update \
                     BEFORE UPDATE OF content_ref ON attachments \
                     BEGIN SELECT 0; END;",
                )
                .unwrap();
        }

        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store.put(b"noop-trigger orphan".to_vec()).await.unwrap();

        let sql = backend.sql();
        let error = store
            .transactional_orphan_sweep(sql.as_ref(), false)
            .await
            .expect_err("sweep must refuse when the fencing triggers are same-named no-ops");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {error:?}"
        );
        assert!(
            store.exists(&orphan).await.unwrap(),
            "a refused sweep must not have deleted anything"
        );

        // The probe must not leave residue behind either.
        let reader = backend.pool().reader().unwrap();
        let leftovers: i64 = reader
            .conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM blob_gc_claims \
                         WHERE root_key GLOB '__fence_probe-*') \
                      + (SELECT COUNT(*) FROM attachments \
                         WHERE record_uuid GLOB '__blob-gc-fence-probe-*')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0, "fence probe rows must not survive the probe");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_probe_refuses_id_collision_and_preserves_the_colliding_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, media_type, created_at) \
                     VALUES ('victim-id', 'entity', 'content', \
                             '2222222222222222222222222222222222222222222222222222222222222222', \
                             'application/test', 7)",
                    [],
                )
                .unwrap();
        }

        let sql = backend.sql();
        let error = super::blob_gc_fence_probe_with_ids(
            sql.as_ref(),
            "victim-id".to_string(),
            "victim-update-id".to_string(),
            "victim-insert2-id".to_string(),
            "victim-update2-id".to_string(),
            "victim-claim-key".to_string(),
        )
        .await
        .expect_err("the probe must refuse when an id it would delete already names a row");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {error:?}"
        );

        let reader = backend.pool().reader().unwrap();
        let (media_type, created_at): (String, i64) = reader
            .conn()
            .query_row(
                "SELECT media_type, created_at FROM attachments \
                 WHERE record_uuid = 'victim-id' AND role = 'content'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("the colliding attachment must survive the refused probe untouched");
        assert_eq!(media_type, "application/test");
        assert_eq!(created_at, 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_probe_does_not_touch_an_unrelated_retained_entity_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            // entities_seq rows intentionally survive entity hard deletion, so
            // an id can collide with the ledger alone — no entities row left
            // for the guard to trip on.
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at) \
                     VALUES ('retained-id', 'local', 'document', 'gone entity', '[]', 7, 7)",
                    [],
                )
                .unwrap();
            writer
                .conn_mut()
                .execute("DELETE FROM entities WHERE id = 'retained-id'", [])
                .unwrap();
            let retained: i64 = writer
                .conn_mut()
                .query_row(
                    "SELECT COUNT(*) FROM entities_seq WHERE entity_id = 'retained-id'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, 1, "fixture requires a retained-only ledger row");
        }

        let sql = backend.sql();
        super::blob_gc_fence_probe_with_ids(
            sql.as_ref(),
            "retained-id".to_string(),
            "retained-update-id".to_string(),
            "retained-insert2-id".to_string(),
            "retained-update2-id".to_string(),
            "retained-claim-key".to_string(),
        )
        .await
        .expect("attachment probe has no reason to mutate an entity sequence row");

        let reader = backend.pool().reader().unwrap();
        let survivors: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM entities_seq WHERE entity_id = 'retained-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            survivors, 1,
            "the retained entity ledger row must survive the attachment probe"
        );
    }

    fn nul_embedded_canonical_ref() -> String {
        let mut polluted = "a".repeat(64);
        polluted.push('\0');
        polluted.push_str("zz");
        polluted
    }

    /// SQLite's `length()` counts characters before the first NUL and GLOB
    /// stops scanning there, so a 64-hex-then-NUL value passes both while the
    /// exact-equality liveness anti-join cannot match it. The byte-length arm
    /// must refuse the sweep on such a claim row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blob_gc_evidence_rejects_a_nul_embedded_claim_ref() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('nul-claim-key', ?1, 0)",
                    rusqlite::params![nul_embedded_canonical_ref()],
                )
                .unwrap();
        }

        let sql = backend.sql();
        let error = super::validate_blob_gc_evidence(sql.as_ref())
            .await
            .expect_err("a NUL-embedded claim ref must refuse the sweep");
        assert!(
            error.to_string().contains("blob_gc_claims"),
            "expected the claims-table refusal, got {error:?}"
        );
    }

    /// The attachments schema CHECK uses the same NUL-blind `length()`/GLOB
    /// pair, so the polluted row INSERTS successfully — this test proves that
    /// on purpose — and the evidence validator must then be the backstop that
    /// refuses the sweep.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blob_gc_evidence_rejects_a_nul_embedded_attachment_ref() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            // The table CHECK now rejects a NUL-embedded ref at admission
            // time; bypass it to simulate a row that reached this state some
            // other way (e.g. a pre-fix legacy row) and prove the validator
            // is still defense-in-depth against it.
            writer
                .conn_mut()
                .execute_batch("PRAGMA ignore_check_constraints = ON")
                .unwrap();
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('nul-attachment-id', 'entity', 'content', ?1, 0)",
                    rusqlite::params![nul_embedded_canonical_ref()],
                )
                .expect("ignore_check_constraints must allow the corrupt row to insert");
            writer
                .conn_mut()
                .execute_batch("PRAGMA ignore_check_constraints = OFF")
                .unwrap();
        }

        let sql = backend.sql();
        let error = super::validate_blob_gc_evidence(sql.as_ref())
            .await
            .expect_err("a NUL-embedded attachment ref must refuse the sweep");
        assert!(
            error.to_string().contains("attachments"),
            "expected the attachments-table refusal, got {error:?}"
        );
    }

    /// A trigger rewrite that fences only the probe's fixed all-zero sentinel
    /// passes the sentinel arms; the second-digest arms must catch it. The
    /// healthy fixture is probed first as the positive control.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_probe_refuses_a_digest_restricted_trigger_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }

        let sql = backend.sql();
        super::blob_gc_fence_probe(sql.as_ref())
            .await
            .expect("the healthy fence must pass all four probe arms");

        {
            let mut writer = backend.pool().writer().unwrap();
            writer
                .conn_mut()
                .execute_batch(
                    "DROP TRIGGER attachments_reject_claimed_blob_insert; \
                     DROP TRIGGER attachments_reject_claimed_blob_update; \
                     CREATE TRIGGER attachments_reject_claimed_blob_insert \
                     BEFORE INSERT ON attachments \
                     WHEN NEW.content_ref = \
                         '0000000000000000000000000000000000000000000000000000000000000000' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END; \
                     CREATE TRIGGER attachments_reject_claimed_blob_update \
                     BEFORE UPDATE OF content_ref ON attachments \
                     WHEN NEW.content_ref = \
                         '0000000000000000000000000000000000000000000000000000000000000000' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END;",
                )
                .unwrap();
        }

        let error = super::blob_gc_fence_probe(sql.as_ref())
            .await
            .expect_err("a sentinel-only fence must fail the second-digest arms");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {error:?}"
        );
        assert!(
            error.to_string().contains("second-digest"),
            "the refusal must name a second-digest arm, got {error}"
        );
    }

    /// A trigger rewrite restricted to the entity/content shape passes the
    /// sentinel arms; the note-shaped second-digest arms must catch it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_probe_refuses_a_shape_restricted_trigger_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute_batch(
                    "DROP TRIGGER attachments_reject_claimed_blob_insert; \
                     DROP TRIGGER attachments_reject_claimed_blob_update; \
                     CREATE TRIGGER attachments_reject_claimed_blob_insert \
                     BEFORE INSERT ON attachments \
                     WHEN NEW.substrate = 'entity' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END; \
                     CREATE TRIGGER attachments_reject_claimed_blob_update \
                     BEFORE UPDATE OF content_ref ON attachments \
                     WHEN NEW.substrate = 'entity' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END;",
                )
                .unwrap();
        }

        let sql = backend.sql();
        let error = super::blob_gc_fence_probe(sql.as_ref())
            .await
            .expect_err("an entity-shape-only fence must fail the note-shaped arms");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {error:?}"
        );
        assert!(
            error.to_string().contains("second-digest"),
            "the refusal must name a second-digest arm, got {error}"
        );
    }

    /// SQLite fixes the text encoding when the database file is first
    /// initialized; creating (and dropping) a table under the pragma leaves
    /// an initialized UTF-16LE database that later writers inherit.
    fn initialize_utf16le_database(db_path: &std::path::Path) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "PRAGMA encoding = 'UTF-16le'; \
             CREATE TABLE __encoding_pin (x INTEGER); \
             DROP TABLE __encoding_pin;",
        )
        .unwrap();
    }

    /// CAST(TEXT AS BLOB) yields the database encoding's bytes, so a fixed
    /// bytes=64 arm would reject every valid 64-char ref in a UTF-16LE
    /// database (128 bytes). The width-derived arm must pass them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blob_gc_evidence_accepts_valid_refs_in_a_utf16le_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        initialize_utf16le_database(&db_path);
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            let encoding: String = writer
                .conn_mut()
                .query_row("PRAGMA encoding", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                encoding, "UTF-16le",
                "the fixture database must actually be UTF-16le"
            );
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            // The fixture copies attachments from the (empty) entities table,
            // so seed one valid canonical row into each scanned table — with
            // no rows the probes measure nothing and a broken byte arm would
            // still pass this test.
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('utf16-valid-attachment', 'entity', 'content', ?1, 0)",
                    rusqlite::params!["a".repeat(64)],
                )
                .unwrap();
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('utf16-valid-claim-key', ?1, 0)",
                    rusqlite::params!["b".repeat(64)],
                )
                .unwrap();
        }

        let sql = backend.sql();
        super::validate_blob_gc_evidence(sql.as_ref())
            .await
            .expect("valid canonical refs must pass in a UTF-16LE database");
    }

    /// The NUL arm must stay red in UTF-16 as well: 64 chars + NUL + tail is
    /// 130+ bytes against the expected 128.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blob_gc_evidence_rejects_a_nul_embedded_claim_ref_in_a_utf16le_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        initialize_utf16le_database(&db_path);
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            let encoding: String = writer
                .conn_mut()
                .query_row("PRAGMA encoding", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                encoding, "UTF-16le",
                "the fixture database must actually be UTF-16le"
            );
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('nul-claim-key-utf16', ?1, 0)",
                    rusqlite::params![nul_embedded_canonical_ref()],
                )
                .unwrap();
        }

        let sql = backend.sql();
        let error = super::validate_blob_gc_evidence(sql.as_ref())
            .await
            .expect_err("a NUL-embedded claim ref must refuse the sweep in UTF-16LE too");
        assert!(
            error.to_string().contains("blob_gc_claims"),
            "expected the claims-table refusal, got {error:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_preserves_put_started_after_liveness_mark() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store.put(b"old orphan".to_vec()).await.unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let (marked, release, _done) = sync_hook::install(&canonical_root);

        let sweep = {
            let store = store.clone();
            let sql = backend.sql();
            tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
        };
        assert!(
            recv_blocking(marked).await,
            "sweep must finish its liveness mark"
        );

        assert!(
            store.write_lock.try_lock().is_err(),
            "the sweep must hold the same root lock used by blob writers"
        );
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let new_ref = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                put_blocking(&root, 0, b"new concurrent blob".to_vec())
            })
        };
        assert!(recv_blocking(started_rx).await, "blob put must start");

        release.send(()).unwrap();
        let sweep_result = sweep.await.unwrap().unwrap();
        let new_ref = new_ref.await.unwrap().unwrap();

        assert_eq!(sweep_result.deleted, 1);
        assert!(!store.exists(&orphan).await.unwrap());
        assert!(
            store.exists(&new_ref).await.unwrap(),
            "a blob put started between the liveness mark and physical sweep must survive"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_releases_sqlite_writer_before_physical_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store
            .put(b"claim then delete outside sqlite".to_vec())
            .await
            .unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let (claimed, release_delete, _done) = sync_hook::install(&canonical_root);

        let sweep = {
            let store = store.clone();
            let sql = backend.sql();
            tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
        };
        assert!(
            recv_blocking(claimed).await,
            "sweep must durably claim the orphan before physical deletion"
        );
        assert!(
            store.exists(&orphan).await.unwrap(),
            "the test seam must pause before the physical delete"
        );
        let external_database_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(database_gc_lock_path(&db_path))
            .unwrap();
        assert!(
            matches!(
                fs4::FileExt::try_lock(&external_database_lock),
                Err(fs4::TryLockError::WouldBlock)
            ),
            "the sweep must retain cross-process database ownership while SQLite's writer is free"
        );

        // The destructive filesystem phase is deliberately paused. An
        // unrelated SQLite writer must nevertheless complete now: this is
        // the hold-time proof that external I/O is no longer inside the
        // sweep's BEGIN IMMEDIATE span.
        let unrelated = rusqlite::Connection::open(&db_path).unwrap();
        unrelated.busy_timeout(Duration::from_millis(100)).unwrap();
        unrelated
            .execute(
                "INSERT INTO entities \
                 (id, namespace, kind, name, tags, created_at, updated_at) \
                 VALUES ('unrelated-writer', 'local', 'concept', 'unrelated', '[]', 1, 1)",
                [],
            )
            .expect("external filesystem work must not retain SQLite's writer lock");

        // The claim trigger is the cross-resource fence: while the file is
        // selected for deletion, a concurrent attachment writer cannot make it
        // newly live in the released-writer window.
        let claimed_err = unrelated
            .execute(
                "INSERT INTO attachments \
                 (record_uuid, substrate, role, content_ref, created_at) \
                 VALUES ('racing-reference', 'entity', 'content', ?1, 1)",
                [orphan.as_str()],
            )
            .expect_err("a claimed content_ref must fail closed before deletion");
        assert!(
            claimed_err.to_string().contains("active blob sweep"),
            "unexpected claim error: {claimed_err}"
        );

        release_delete.send(()).unwrap();
        let result = sweep.await.unwrap().unwrap();
        assert_eq!(result.deleted, 1);
        assert!(!store.exists(&orphan).await.unwrap());

        let remaining_claims: i64 = unrelated
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims WHERE content_ref = ?1",
                [orphan.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining_claims, 0,
            "successful deletion releases the claim"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_sweep_during_delete_keeps_owner_locks_until_blocking_work_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store.put(b"cancelled sweep orphan".to_vec()).await.unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let (claimed, release_delete, done) = sync_hook::install(&canonical_root);

        let sweep = {
            let store = store.clone();
            let sql = backend.sql();
            tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
        };
        assert!(recv_blocking(claimed).await);
        sweep.abort();
        assert!(sweep.await.unwrap_err().is_cancelled());

        let external_root_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(ROOT_WRITE_LOCK_FILE))
            .unwrap();
        let external_database_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(database_gc_lock_path(&db_path))
            .unwrap();
        assert!(matches!(
            fs4::FileExt::try_lock(&external_root_lock),
            Err(fs4::TryLockError::WouldBlock)
        ));
        assert!(matches!(
            fs4::FileExt::try_lock(&external_database_lock),
            Err(fs4::TryLockError::WouldBlock)
        ));

        release_delete.send(()).unwrap();
        let done_disconnected = tokio::task::spawn_blocking(move || done.recv().is_err())
            .await
            .unwrap();
        assert!(
            done_disconnected,
            "the cancelled outer task cannot send done"
        );
        assert!(fs4::FileExt::try_lock(&external_root_lock).is_ok());
        assert!(fs4::FileExt::try_lock(&external_database_lock).is_ok());
        drop(external_root_lock);
        drop(external_database_lock);
        assert!(!store.exists(&orphan).await.unwrap());
        let stranded_claims: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            stranded_claims, 1,
            "cancellation leaves a fail-closed claim"
        );

        let recovered = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(recovered.deleted, 0);
        let remaining: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "the next exclusive owner recovers the claim");
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_recovers_stale_claims_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));
        let bytes = b"republished after a crashed claim".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let root_key = blob_root_key(&canonical_root);
        let absent_ref = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let former_probe_seed = "1111111111111111111111111111111111111111111111111111111111111111";
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES (?1, ?2, 1), (?1, ?3, 1), (?1, ?4, 1)",
                    rusqlite::params![
                        root_key,
                        content_ref.as_str(),
                        absent_ref,
                        former_probe_seed
                    ],
                )
                .unwrap();
        }

        // A publisher that recovered after the claiming process crashed
        // refreshes the digest's grace witness before its attachment write. The
        // next sweep must clear this protected claim, the claim whose file was
        // already removed, and the former fixed probe seed without letting a
        // healthy attachment fence block abandoned-claim recovery forever.
        assert_eq!(store.put(bytes).await.unwrap(), content_ref);
        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(result.deleted, 0);
        assert_eq!(result.grace_period_skipped, 1);
        assert!(store.exists(&content_ref).await.unwrap());

        let remaining: i64 = backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims WHERE root_key = ?1",
                [blob_root_key(&canonical_root)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "the next sweep recovers stale claims");
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_recovers_claims_after_root_relocation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }

        let old_root = dir.path().join("old-blobs");
        let bytes = b"claim must follow a relocated blob root".to_vec();
        let content_ref = {
            let old_store = FsBlobStore::new(old_root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::from_secs(60));
            old_store.put(bytes).await.unwrap()
        };
        let old_root_key = blob_root_key(&old_root.canonicalize().unwrap());
        backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                 VALUES (?1, ?2, 1)",
                rusqlite::params![old_root_key, content_ref.as_str()],
            )
            .unwrap();

        let new_root = dir.path().join("relocated-blobs");
        std::fs::rename(&old_root, &new_root).unwrap();
        let relocated_store = FsBlobStore::new(new_root, 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));
        let result = relocated_store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();

        assert_eq!(
            result.deleted, 0,
            "a fresh relocated blob remains protected"
        );
        assert_eq!(result.grace_period_skipped, 1);
        assert!(relocated_store.exists(&content_ref).await.unwrap());
        let remaining: i64 = backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            remaining, 0,
            "exclusive database sweep ownership makes every pre-existing claim abandoned, \
             even when its old path-derived root key no longer matches"
        );
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_recovers_claims_copied_by_database_restore() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.db");
        let restored_path = dir.path().join("restored.db");
        let bytes = b"claim copied in an online database backup".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        {
            let source = crate::StorageBackend::sqlite(&source_path).unwrap();
            let mut writer = source.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('source-root-before-backup', ?1, 1)",
                    [content_ref.as_str()],
                )
                .unwrap();
            writer
                .conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        std::fs::copy(&source_path, &restored_path).unwrap();

        let restored = crate::StorageBackend::sqlite(&restored_path).unwrap();
        let restored_root = dir.path().join("restored-blobs");
        let store = FsBlobStore::new(restored_root, 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));
        assert_eq!(store.put(bytes).await.unwrap(), content_ref);
        let result = store
            .transactional_orphan_sweep(restored.sql().as_ref(), false)
            .await
            .unwrap();

        assert_eq!(result.deleted, 0);
        assert_eq!(result.grace_period_skipped, 1);
        let remaining: i64 = restored
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "restored claims are abandoned ownership");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_bounds_each_durable_claim_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let candidate_count = BLOB_GC_CLAIM_BATCH_SIZE * 2 + 1;
        for index in 0..candidate_count {
            store
                .put(format!("bounded claim candidate {index}").into_bytes())
                .await
                .unwrap();
        }
        let canonical_root = root.canonicalize().unwrap();
        let (claimed, release_delete, _done) = sync_hook::install(&canonical_root);

        let sweep = {
            let store = store.clone();
            let sql = backend.sql();
            tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
        };
        assert!(
            recv_blocking(claimed).await,
            "the first bounded claim batch must commit before deletion"
        );
        let active_claims: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert!(active_claims > 0);
        assert!(
            active_claims <= BLOB_GC_CLAIM_BATCH_SIZE as i64,
            "one transaction may expose at most {BLOB_GC_CLAIM_BATCH_SIZE} claim rows; \
             observed {active_claims}"
        );

        release_delete.send(()).unwrap();
        let result = sweep.await.unwrap().unwrap();
        assert_eq!(result.deleted, candidate_count as u64);
        let remaining: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn abandoned_claim_recovery_deletes_at_most_one_batch_per_writer_hold() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            crate::run_migrations(writer.conn_mut()).unwrap();
            let tx = writer.conn_mut().transaction().unwrap();
            for index in 0..(BLOB_GC_CLAIM_BATCH_SIZE + 1) {
                tx.execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-root', ?1, 1)",
                    [format!("{index:064x}")],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        let released = release_abandoned_blob_gc_claim_batch(backend.sql().as_ref())
            .await
            .unwrap();
        assert_eq!(released, BLOB_GC_CLAIM_BATCH_SIZE as u64);
        let remaining: i64 = backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_refuses_corrupt_liveness_and_claim_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        let orphan = store
            .put(b"must survive corrupt evidence".to_vec())
            .await
            .unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        conn.execute(
            "INSERT INTO attachments \
             (record_uuid, substrate, role, content_ref, created_at) \
             VALUES ('corrupt-live', 'entity', 'content', 'not-a-content-ref', 1)",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
            .unwrap();
        let live_error = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .expect_err("corrupt live evidence must fail closed");
        assert!(matches!(live_error, StorageError::InvalidInput { .. }));
        assert!(
            store.exists(&orphan).await.unwrap(),
            "no file may be removed after corrupt live evidence"
        );

        conn.execute(
            "DELETE FROM attachments WHERE record_uuid = 'corrupt-live'",
            [],
        )
        .unwrap();
        let root_key = blob_root_key(&root.canonicalize().unwrap());
        conn.execute(
            "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
             VALUES (?1, 'also-not-a-content-ref', 1)",
            [root_key.as_str()],
        )
        .unwrap();
        let claim_error = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .expect_err("corrupt durable claim evidence must fail closed");
        assert!(matches!(claim_error, StorageError::InvalidInput { .. }));
        assert!(
            store.exists(&orphan).await.unwrap(),
            "no file may be removed after corrupt claim evidence"
        );
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = ?1 AND content_ref = 'also-not-a-content-ref'",
                [root_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remaining, 1,
            "corrupt claim evidence is not silently erased"
        );
        let probe_residue: i64 = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM blob_gc_claims \
                         WHERE root_key GLOB '__fence_probe-*') \
                      + (SELECT COUNT(*) FROM attachments \
                         WHERE record_uuid GLOB '__blob-gc-fence-probe-*')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            probe_residue, 0,
            "invalid evidence must abort before the functional fence probe"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_republishes_deduplicated_external_put() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let payload = b"existing orphan republished during sweep".to_vec();
        let orphan = store.put(payload.clone()).await.unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let (marked, release, _done) = sync_hook::install(&canonical_root);

        let sweep = {
            let store = store.clone();
            let sql = backend.sql();
            tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
        };
        assert!(
            recv_blocking(marked).await,
            "sweep must finish its liveness mark"
        );

        let external_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(ROOT_WRITE_LOCK_FILE))
            .unwrap();
        assert!(
            matches!(
                fs4::FileExt::try_lock(&external_lock),
                Err(fs4::TryLockError::WouldBlock)
            ),
            "the sweep must exclude a publisher using an independently opened root lock"
        );

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let republished = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                put_blocking(&root, 0, payload)
            })
        };
        assert!(recv_blocking(started_rx).await, "blob put must start");

        release.send(()).unwrap();
        let sweep_result = sweep.await.unwrap().unwrap();
        let republished = republished.await.unwrap().unwrap();

        assert_eq!(sweep_result.deleted, 1);
        assert_eq!(republished, orphan);
        assert!(
            store.exists(&republished).await.unwrap(),
            "a deduplicated put concurrent with the sweep must not return a deleted reference"
        );
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_uses_all_attachment_refs_as_live() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        let live = store.put(b"live".to_vec()).await.unwrap();
        let soft_deleted = store.put(b"soft deleted".to_vec()).await.unwrap();
        let orphan = store.put(b"orphan".to_vec()).await.unwrap();
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                     VALUES ('live', 'local', 'document', 'live', '[]', 1, 1, NULL), \
                            ('deleted', 'local', 'document', 'deleted', '[]', 1, 1, 2);",
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('live', 'entity', 'content', ?1, 1), \
                            ('deleted', 'entity', 'content', ?2, 1)",
                    rusqlite::params![live.as_str(), soft_deleted.as_str()],
                )
                .unwrap();
        }

        let dry_run = store
            .transactional_orphan_sweep(backend.sql().as_ref(), true)
            .await
            .unwrap();
        assert_eq!(dry_run.would_delete, 1);
        assert_eq!(dry_run.deleted, 0);
        assert!(store.exists(&soft_deleted).await.unwrap());
        assert!(store.exists(&orphan).await.unwrap());

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();

        assert_eq!(result.scanned, 3);
        assert_eq!(result.deleted, 1);
        assert!(store.exists(&live).await.unwrap());
        assert!(
            store.exists(&soft_deleted).await.unwrap(),
            "soft delete retains attachment rows and their blobs"
        );
        assert!(!store.exists(&orphan).await.unwrap());
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_protects_a_freshly_published_blob_before_its_reference_commits(
    ) {
        // The exact two-step client protocol hazard: `put` completes and
        // releases its write lock (step 1) while the attachment write that will
        // *later* commit a `content_ref` to this blob (step 2) has not
        // happened yet -- nothing in this store's locking serializes the
        // two, because they are separate calls the client makes with an
        // arbitrary gap in between. A sweep that lands in that gap must not
        // delete the blob: `attachments.content_ref` has no row for it yet
        // purely because the referencing write hasn't landed, not because
        // it is actually orphaned. Without the publish-grace window this
        // reproduces khive#1313's dangling-reference defect: the blob file
        // is deleted here, and the still-pending attachment write below would
        // commit a `content_ref` to nothing.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        // Default (non-zero) grace period -- this test exercises exactly
        // what it exists to protect.
        let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();

        // Step 1: put completes, lock released. No attachment anywhere
        // references this blob yet.
        let blob = store
            .put(b"published, reference not yet committed".to_vec())
            .await
            .unwrap();

        // A sweep runs in the gap before step 2 (the attachment write) happens.
        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();

        assert_eq!(result.deleted, 0, "the blob must survive: {result:?}");
        assert_eq!(
            result.would_delete, 0,
            "not treated as a deletable orphan: {result:?}"
        );
        assert_eq!(
            result.grace_period_skipped, 1,
            "must be reported as grace-protected rather than silently ignored: {result:?}"
        );
        assert!(
            store.exists(&blob).await.unwrap(),
            "a blob still inside its publish grace period must survive the sweep"
        );

        // Step 2 now lands: the record-plus-attachment write commits content_ref to the
        // still-present blob.
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                     VALUES ('e1', 'local', 'document', 'e1', '[]', 1, 1, NULL)",
                    [],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('e1', 'entity', 'content', ?1, 1)",
                    [blob.as_str()],
                )
                .unwrap();
        }

        // A later sweep now finds it live and keeps it for the ordinary
        // reason, independent of the grace window.
        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(result.deleted, 0);
        assert!(store.exists(&blob).await.unwrap());
    }

    #[tokio::test]
    async fn put_republishing_an_aged_orphan_restarts_its_grace_clock_before_the_reference_commits()
    {
        // The dedup fast path (`target.exists()`) used to return without
        // touching the file at all -- so a stale, already-orphaned blob
        // re-published by an identical `put` kept its OLD mtime, bypassed
        // the publish-grace check, and a transactional sweep landing in the
        // gap before the caller's follow-up attachment write could delete it
        // out from under that write (khive#1313). This reproduces the
        // race end to end and proves the mtime refresh closes it.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));

        let bytes = b"old orphan re-published".to_vec();
        let first = store.put(bytes.clone()).await.unwrap();

        // Age the blob well past the 60s grace floor -- no sleeps, same
        // backdating pattern as the existing older-than-grace test.
        let path = shard_path(store.root(), &first);
        let old_mtime = SystemTime::now() - Duration::from_secs(3600);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();

        // A deduplicating put republishes the identical bytes. No attachment
        // anywhere references this blob yet.
        let second = store.put(bytes).await.unwrap();
        assert_eq!(first, second);

        // The sweep lands in the gap before the follow-up attachment write --
        // the refreshed mtime must keep it inside the grace window.
        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(
            result.deleted, 0,
            "a dedup-republished blob must survive a sweep landing before its reference \
             commits: {result:?}"
        );
        assert_eq!(
            result.grace_period_skipped, 1,
            "must be reported as grace-protected, not silently ignored: {result:?}"
        );
        assert!(store.exists(&first).await.unwrap());

        // The caller's follow-up record-plus-attachment write now lands.
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                     VALUES ('e1', 'local', 'document', 'e1', '[]', 1, 1, NULL)",
                    [],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('e1', 'entity', 'content', ?1, 1)",
                    [first.as_str()],
                )
                .unwrap();
        }

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(result.deleted, 0);
        assert!(
            store.exists(&first).await.unwrap(),
            "the blob must stay live once its reference has committed"
        );
    }

    #[tokio::test]
    async fn put_dedup_mtime_refresh_has_no_observable_effect_under_zero_grace_period() {
        // The assumption the fix relies on for every zero-grace test in this
        // file: `within_publish_grace` with `Duration::ZERO` never protects a
        // candidate regardless of its mtime (`age < Duration::ZERO` is always
        // false), so refreshing the mtime on a deduplicated republish must
        // not change zero-grace sweep behavior. Verified directly rather
        // than assumed.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        let bytes = b"zero grace dedup refresh".to_vec();
        let first = store.put(bytes.clone()).await.unwrap();
        let second = store.put(bytes.clone()).await.unwrap();
        assert_eq!(first, second);

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(
            result.deleted, 1,
            "a zero grace period must still delete an unreferenced blob even after a dedup \
             put refreshed its mtime: {result:?}"
        );
        assert!(!store.exists(&first).await.unwrap());
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_still_removes_orphans_older_than_the_grace_period() {
        // The grace window narrows the publish-vs-sweep race, it does not
        // disable sweeping outright: an object whose age already exceeds a
        // (short, for this test) grace period is removed exactly as before,
        // proving the fix bounds the exposure rather than papering over it.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));

        let orphan = store
            .put(b"actually orphaned, published long ago".to_vec())
            .await
            .unwrap();
        // Back-date the file's mtime well past the 60s grace period instead
        // of sleeping in the test.
        let path = shard_path(store.root(), &orphan);
        let old_mtime = SystemTime::now() - Duration::from_secs(3600);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();

        assert_eq!(
            result.deleted, 1,
            "an orphan older than the grace period must still be swept: {result:?}"
        );
        assert_eq!(result.grace_period_skipped, 0);
        assert!(!store.exists(&orphan).await.unwrap());
    }

    #[test]
    fn resolve_blob_root_prefers_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("KHIVE_BLOB_ROOT", "/tmp/env-override-root");
        let resolved = resolve_blob_root(Some(Path::new("/db/dir")), Some(Path::new("/cfg/root")));
        std::env::remove_var("KHIVE_BLOB_ROOT");
        assert_eq!(resolved.unwrap(), PathBuf::from("/tmp/env-override-root"));
    }

    #[test]
    fn resolve_blob_root_prefers_config_over_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KHIVE_BLOB_ROOT");
        let resolved = resolve_blob_root(Some(Path::new("/db/dir")), Some(Path::new("/cfg/root")));
        assert_eq!(resolved.unwrap(), PathBuf::from("/cfg/root"));
    }

    #[test]
    fn resolve_blob_root_defaults_beside_db_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KHIVE_BLOB_ROOT");
        let resolved = resolve_blob_root(Some(Path::new("/db/dir")), None);
        assert_eq!(resolved.unwrap(), PathBuf::from("/db/dir/blobs"));
    }

    #[test]
    fn resolve_blob_root_errors_with_no_env_config_or_db_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KHIVE_BLOB_ROOT");
        let resolved = resolve_blob_root(None, None);
        assert!(resolved.is_err());
    }

    // `std::env::set_var`/`remove_var` mutate real process-global state, so the
    // four `resolve_blob_root` env-precedence tests must not interleave under
    // the crate's default parallel test runner.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_walk_ignores_a_leaf_swapped_for_an_outside_symlink_mid_scan(
    ) {
        // Regression for the PR #2201 review finding: the sweep's candidate
        // walk and grace-period mtime read used to be two separate,
        // path-based passes (`walk_blob_files` then `within_publish_grace`
        // via `fs::metadata(path)`), neither pinned to the retained
        // `root_handle`. A concurrent replacement of a leaf entry in the gap
        // between those passes could make the mtime read observe a file
        // OUTSIDE the retained root, letting a stale outside mtime evict the
        // grace period for a freshly published in-root blob. The fix folds
        // discovery and classification into one handle-relative
        // openat(..., O_NOFOLLOW) + fstat in
        // `walk_blob_files_from_root_handle`, so there is no later path
        // re-resolution left to race. This test forces exactly that swap —
        // via `walk_leaf_sync_hook`, between the leaf's hex-name discovery
        // and its classifying open — and proves the outside decoy's stale
        // mtime never reaches the sweep's counts.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }

        let root = dir.path().join("blobs");
        let store = std::sync::Arc::new(
            FsBlobStore::new(root.clone(), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::from_secs(3600)),
        );

        // The one real, unreferenced candidate the sweep will discover. Its
        // grace period is wide (1h) so correct handle-relative
        // classification always protects it.
        let real = store
            .put(b"real freshly-published blob".to_vec())
            .await
            .unwrap();
        let real_path = shard_path(&root, &real);

        // Outside decoy sharing the SAME leaf name (content-ref hex), aged
        // far past the grace period. If classification ever reads through
        // a swapped symlink, the candidate looks like a stale, deletable
        // orphan instead of a protected fresh publish.
        let outside_dir = dir.path().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        let decoy_path = outside_dir.join(real.as_str());
        fs::write(&decoy_path, b"outside decoy, must never be observed").unwrap();
        let ancient = SystemTime::now() - Duration::from_secs(7200);
        fs::OpenOptions::new()
            .write(true)
            .open(&decoy_path)
            .unwrap()
            .set_modified(ancient)
            .unwrap();

        let (reached, release) = walk_leaf_sync_hook::install(&root);
        let sweep = {
            let store = store.clone();
            let sql = backend.sql();
            tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), true).await })
        };
        assert!(
            recv_blocking(reached).await,
            "sweep walk must reach the leaf classification pause"
        );

        // Swap the real leaf out from under the paused walk: same name, now
        // a symlink resolving outside the retained root.
        fs::remove_file(&real_path).unwrap();
        std::os::unix::fs::symlink(&decoy_path, &real_path).unwrap();

        release.send(()).unwrap();
        let result = sweep.await.unwrap().unwrap();

        assert_eq!(
            result.would_delete, 0,
            "an outside decoy's stale mtime must never make an in-root candidate \
             eligible for deletion: {result:?}"
        );
        assert_eq!(
            result.grace_period_skipped, 0,
            "the swapped leaf is a symlink; `openat(..., O_NOFOLLOW)` refuses it, so it \
             must be dropped from candidates entirely rather than counted (real or \
             outside) at all: {result:?}"
        );
        assert_eq!(
            result.scanned, 0,
            "the symlinked leaf must never be scanned as a candidate: {result:?}"
        );

        // Restore a real leaf and confirm the walk is not permanently wedged
        // by the hook: an un-swapped root still finds and reports a real,
        // grace-protected candidate normally.
        fs::remove_file(&real_path).unwrap();
        fs::write(&real_path, b"real freshly-published blob").unwrap();
        let control = store
            .transactional_orphan_sweep(backend.sql().as_ref(), true)
            .await
            .unwrap();
        assert_eq!(
            control.grace_period_skipped, 1,
            "control: the un-replaced root must still classify the real candidate as \
             grace-protected: {control:?}"
        );
        assert_eq!(control.would_delete, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_walk_still_finds_a_real_orphan_past_its_grace_period() {
        // Control for the swap test above, using an independent root (no
        // hook installed): the handle-relative walk must still classify and
        // report a genuine past-grace orphan as deletable, proving the fix
        // narrows the walk to descriptor-relative reads without disabling
        // orphan detection itself.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
        }
        let root = dir.path().join("blobs");
        let store = FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));

        let orphan = store.put(b"aged real orphan".to_vec()).await.unwrap();
        let path = shard_path(&root, &orphan);
        let ancient = SystemTime::now() - Duration::from_secs(3600);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(ancient)
            .unwrap();

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();
        assert_eq!(
            result.deleted, 1,
            "a real orphan older than the grace period must still be swept: {result:?}"
        );
        assert!(!store.exists(&orphan).await.unwrap());
    }
}
