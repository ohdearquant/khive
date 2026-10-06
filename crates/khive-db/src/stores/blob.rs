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
//! partially-committed blob). On Unix, publication also synchronizes the
//! shard directory chain before acknowledging the reference. Initialization
//! synchronizes the root and its parent, which must already exist and be
//! durable. This is not a power-loss guarantee for arbitrary filesystems or
//! devices. Non-Unix publication does not provide these directory barriers.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

#[cfg(unix)]
use khive_fs::fd_relative::{clear_errno, current_errno};
use khive_storage::blob::{
    BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef, UploadId,
    UploadLeaseConfig, MAX_BLOB_WHOLE_BYTES,
};
use khive_storage::error::StorageError;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue, StorageResult};
use khive_storage::{AtomicUnitOp, SqlAccess, StorageCapability};

use crate::error::SqliteError;
use uuid::Uuid;

mod root_handle;
#[cfg(windows)]
use root_handle::open_blob_shard_file_no_follow_windows;
pub use root_handle::resolve_blob_root;
#[cfg(unix)]
use root_handle::{
    acquire_root_write_lock_at, available_space_at, create_regular_file_at_no_follow,
    open_or_create_dir_at_no_follow, openat_dir_no_follow, openat_regular_file_no_follow,
    rename_entry_at, unlink_entry_at,
};
use root_handle::{
    blob_root_key, open_blob_root_handle, open_blob_shard_file_no_follow,
    unlink_blob_shard_file_no_follow, verify_blob_root_identity,
};

#[path = "blob_uploads.rs"]
mod uploads;
#[path = "blob_write_locks.rs"]
mod write_locks;
use write_locks::write_lock_for_root;

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
            required_headroom_bytes: required_write_bytes,
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

    let temporary = tmp.into_temp_path();
    publish_blob_path(&temporary, &target)?;

    Ok(content_ref)
}

#[cfg(any(test, not(unix)))]
fn publish_blob_path(source: &Path, target: &Path) -> StorageResult<()> {
    fs::rename(source, target).map_err(|error| map_io_err(error, "put_persist"))
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
struct BlobPublication {
    #[cfg(test)]
    hook: Option<sync_hook::Publication>,
}

#[cfg(unix)]
impl BlobPublication {
    fn step<T>(
        &self,
        operation: &'static str,
        action: impl FnOnce() -> std::io::Result<T>,
    ) -> StorageResult<T> {
        self.io_step(operation, action)
            .map_err(|error| map_io_err(error, operation))
    }

    fn io_step<T>(
        &self,
        _operation: &'static str,
        action: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            hook.before(_operation)?;
        }
        let result = action()?;
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            hook.completed(_operation);
        }
        Ok(result)
    }

    fn sync_directories(
        &self,
        root: &fs::File,
        shard1: &fs::File,
        shard2: &fs::File,
    ) -> StorageResult<()> {
        // Existing directories may come from an interrupted publication, so
        // their presence cannot discharge a previous attempt's barriers.
        for (operation, directory) in [
            ("put_sync_shard", shard2),
            ("put_sync_parent", shard1),
            ("put_sync_root", root),
        ] {
            self.sync_directory(operation, directory)
                .map_err(|error| map_io_err(error, operation))?;
        }
        Ok(())
    }

    fn sync_directory(&self, operation: &'static str, directory: &fs::File) -> std::io::Result<()> {
        self.io_step(operation, || {
            sync_directory(directory)?;
            #[cfg(test)]
            if let Some(hook) = &self.hook {
                hook.directory_synced(operation, directory)?;
            }
            Ok(())
        })
    }
}

#[cfg(unix)]
fn sync_directory(directory: &fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        // Keep authority on the opened directory; no pathname is resolved
        // again. File::sync_all uses F_FULLFSYNC on Apple, whereas this
        // barrier specifically requests directory metadata persistence.
        if unsafe { libc::fsync(directory.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(unix)]
fn publish_blob_at(
    root: &fs::File,
    shard1: &fs::File,
    shard2: &fs::File,
    source: &fs::File,
    temp_name: &str,
    content_ref: &ContentRef,
    publication: &BlobPublication,
) -> StorageResult<()> {
    use std::os::fd::AsRawFd;

    if let Err(error) = publication.step("put_persist", || {
        rename_entry_at(
            source.as_raw_fd(),
            temp_name,
            shard2.as_raw_fd(),
            content_ref.as_str(),
        )
    }) {
        let _ = unlink_entry_at(source.as_raw_fd(), temp_name);
        return Err(error);
    }
    // A barrier failure after rename leaves a complete but unacknowledged
    // object. Do not delete it: readers may already hold its reference.
    publication.sync_directories(root, shard1, shard2)
}

#[cfg(unix)]
fn put_blocking_from_root_handle(
    root: &Path,
    root_handle: &std::fs::File,
    floor_bytes: u64,
    bytes: Vec<u8>,
    publication: &BlobPublication,
) -> StorageResult<ContentRef> {
    use std::os::unix::io::AsRawFd;

    let _root_write_guard = acquire_root_write_lock_anchored(root, root_handle)?;
    let digest = blake3::hash(&bytes);
    let content_ref = ContentRef::from_digest_bytes(digest.as_bytes());

    // Preserve the dedup/republish contract while opening the existing leaf
    // relative to the retained root. A missing shard level and a missing leaf
    // are both the ordinary publish path; every other traversal failure is a
    // hard refusal.
    let hex = content_ref.as_str();
    let existing = (|| -> std::io::Result<_> {
        let shard1 = openat_dir_no_follow(root_handle.as_raw_fd(), &hex[0..2])?;
        let shard2 = openat_dir_no_follow(shard1.as_raw_fd(), &hex[2..4])?;
        let file = openat_regular_file_no_follow(shard2.as_raw_fd(), hex, libc::O_WRONLY)?;
        Ok((file, shard1, shard2))
    })();
    match existing {
        Ok((file, shard1, shard2)) => {
            file.set_modified(SystemTime::now())
                .map_err(|e| map_io_err(e, "put_touch_mtime"))?;
            publication.step("put_fsync", || file.sync_all())?;
            publication.sync_directories(root_handle, &shard1, &shard2)?;
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
            required_headroom_bytes: required_write_bytes,
        });
    }

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
        publication.step("put_fsync", || temp.sync_all())?;

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

    publish_blob_at(
        root_handle,
        &shard1_dir,
        &shard2_dir,
        &shard2_dir,
        &temp_name,
        &content_ref,
        publication,
    )?;
    Ok(content_ref)
}

#[cfg(not(unix))]
fn put_blocking_from_root_handle(
    root: &Path,
    root_handle: &std::fs::File,
    floor_bytes: u64,
    bytes: Vec<u8>,
) -> StorageResult<ContentRef> {
    // This path retains file synchronization and atomic publication but does
    // not claim the Unix directory-metadata persistence barrier.
    verify_blob_root_identity(root, root_handle).map_err(|e| map_io_err(e, "put_root_identity"))?;
    put_blocking(root, floor_bytes, bytes)
}

#[cfg(unix)]
fn refresh_publish_grace_blocking(
    root: &Path,
    root_handle: &fs::File,
    content_ref: &ContentRef,
) -> StorageResult<Option<u64>> {
    use std::os::fd::AsRawFd;

    let _root_write_guard = acquire_root_write_lock_anchored(root, root_handle)?;
    let hex = content_ref.as_str();
    let opened = (|| -> std::io::Result<_> {
        let shard1 = openat_dir_no_follow(root_handle.as_raw_fd(), &hex[..2])?;
        let shard2 = openat_dir_no_follow(shard1.as_raw_fd(), &hex[2..4])?;
        let file = openat_regular_file_no_follow(shard2.as_raw_fd(), hex, libc::O_WRONLY)?;
        Ok(file)
    })();
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(map_io_err(error, "refresh_publish_grace_open")),
    };
    let size = file
        .metadata()
        .map_err(|error| map_io_err(error, "refresh_publish_grace_stat"))?
        .len();
    file.set_modified(SystemTime::now())
        .map_err(|error| map_io_err(error, "refresh_publish_grace_mtime"))?;
    file.sync_all()
        .map_err(|error| map_io_err(error, "refresh_publish_grace_fsync"))?;
    Ok(Some(size))
}

#[cfg(windows)]
fn refresh_publish_grace_blocking(
    root: &Path,
    root_handle: &fs::File,
    content_ref: &ContentRef,
) -> StorageResult<Option<u64>> {
    let _root_write_guard = acquire_root_write_lock_anchored(root, root_handle)?;
    let file = match open_blob_shard_file_no_follow_windows(root, root_handle, content_ref, true) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(map_io_err(error, "refresh_publish_grace_open")),
    };
    let size = file
        .metadata()
        .map_err(|error| map_io_err(error, "refresh_publish_grace_stat"))?
        .len();
    file.set_modified(SystemTime::now())
        .map_err(|error| map_io_err(error, "refresh_publish_grace_mtime"))?;
    file.sync_all()
        .map_err(|error| map_io_err(error, "refresh_publish_grace_fsync"))?;
    Ok(Some(size))
}

#[cfg(not(any(unix, windows)))]
fn refresh_publish_grace_blocking(
    _root: &Path,
    _root_handle: &fs::File,
    _content_ref: &ContentRef,
) -> StorageResult<Option<u64>> {
    Err(StorageError::Unsupported {
        capability: StorageCapability::Blob,
        operation: "refresh_publish_grace".into(),
        message: "safe in-place mtime refresh is unsupported on this platform".into(),
    })
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
                #[cfg(all(test, unix))]
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

    // The sweep is admitted only for the EXACT completed V21 epoch
    // (ADR-160 Phase 4a: "report-only and destructive sweeps only for an
    // exact completed V21 epoch … ahead-of-V21 epochs return typed
    // Unsupported"). A ledger above V21 — whether ahead of this binary or a
    // migration this same binary applied — belongs to a schema epoch whose
    // attachment/liveness semantics this gate never validated, so it fails
    // closed; the author of a future migration extends the gate in the same
    // change that proves the new epoch's liveness set, never by default.
    // General schema validation (`attachment_cutover_status`) deliberately
    // keeps accepting later versions on top of a completed cutover: that is
    // the normal serving course, and the exact-epoch rule is scoped to
    // destructive GC admission.
    //
    // "Exact completed V21 epoch" means the WHOLE canonical ledger, not a
    // V21 terminal row: `version` is the table's PRIMARY KEY, so
    // COUNT(*) = 21 ∧ MIN = 1 ∧ MAX = 21 holds if and only if the ledger is
    // exactly the contiguous set {1..21}. A ledger that merely retains a V21
    // row at MAX(version) = 21 while earlier rows are missing is an
    // incomplete migration history whose physical schema this gate never
    // validated — it must fail closed, same as ahead-of-V21. Name-level
    // canonicality of the below-terminal rows stays boot's job
    // (`validate_applied_migration_ledger`); this predicate enforces the
    // structural contiguity a destructive sweep's admission rests on, plus
    // the named V21 row itself.
    let complete = required_nonnegative_count(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachment_cutover_state AS cutover \
                      WHERE cutover.singleton = 1 \
                        AND cutover.state = 'complete' \
                        AND cutover.completed_at IS NOT NULL \
                        AND (SELECT COUNT(*) FROM _schema_migrations \
                             WHERE version = ?1 \
                               AND name = 'attachments_first_class') = 1 \
                        AND (SELECT COUNT(*) FROM _schema_migrations) = ?1 \
                        AND (SELECT MIN(version) FROM _schema_migrations) = 1 \
                        AND (SELECT MAX(version) FROM _schema_migrations) = ?1"
                    .to_string(),
                params: vec![SqlValue::Integer(i64::from(
                    crate::migrations::ATTACHMENT_CUTOVER_VERSION,
                ))],
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

/// Candidate ownership for every GC accounting and claim site.
///
/// The exact-V21 admission gate remains unchanged. Its legacy schema has no
/// quarantine table, so callers select the canonical-only fragment when that
/// table is absent rather than preparing a reference to a missing table.
fn blob_gc_unowned_attachment_predicate(quarantine_present: bool) -> &'static str {
    if quarantine_present {
        "NOT EXISTS ( \
           SELECT 1 FROM attachments \
           WHERE content_ref = candidate.value \
         ) AND NOT EXISTS ( \
           SELECT 1 FROM attachment_quarantine \
           WHERE content_ref = candidate.value \
         )"
    } else {
        "NOT EXISTS ( \
           SELECT 1 FROM attachments \
           WHERE content_ref = candidate.value \
         )"
    }
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
            let quarantine_present = required_nonnegative_count(
                writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM sqlite_master \
                              WHERE type = 'table' AND name = 'attachment_quarantine'"
                            .to_string(),
                        params: vec![],
                        label: Some("blob_gc_quarantine_table_present".to_string()),
                    })
                    .await?,
                "blob_gc_quarantine_table_present",
            )?;
            let ownership_predicate = match quarantine_present {
                0 => blob_gc_unowned_attachment_predicate(false),
                1 => blob_gc_unowned_attachment_predicate(true),
                _ => {
                    return Err(StorageError::Internal(
                        "blob GC quarantine table presence returned an invalid count".into(),
                    ));
                }
            };
            let grace_period_skipped = required_nonnegative_count(
                writer
                    .query_scalar(SqlStatement {
                        sql: format!(
                            "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                             WHERE {ownership_predicate}"
                        ),
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
                            sql: format!(
                                "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                                 WHERE {ownership_predicate}"
                            ),
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
                    sql: format!(
                        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                         SELECT ?1, candidate.value, ?3 \
                         FROM json_each(?2) AS candidate \
                         WHERE {ownership_predicate}"
                    ),
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
    upload_observations: Arc<StdMutex<uploads::lease::Observations>>,
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

    /// Create a store rooted at `root`, creating only that directory if absent.
    /// Its parent must already exist and be durable. Missing ancestors are
    /// refused rather than becoming an unverified anchor after a failed retry.
    /// On Unix every call synchronizes the root and its parent, including when
    /// the root already exists. `open_existing` performs no such mutation.
    pub fn new(root: PathBuf, floor_bytes: u64) -> Result<Self, SqliteError> {
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = root
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("blob root parent missing: {}", parent.display()),
                )
                .into());
            }
            Err(error) => return Err(error.into()),
        }
        let store = Self::open_existing(root, floor_bytes)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let publication = BlobPublication {
                #[cfg(test)]
                hook: sync_hook::take(&store.root).and_then(|hook| hook.publication),
            };
            let parent = openat_dir_no_follow(store.root_handle.as_raw_fd(), "..")?;
            publication.sync_directory("init_sync_root", &store.root_handle)?;
            publication.sync_directory("init_sync_parent", &parent)?;
        }
        Ok(store)
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
            upload_observations: Arc::default(),
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
    async fn refresh_publish_grace(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        // Keep the owned async guard with the blocking work even when the
        // caller is cancelled; the same root lock serializes put and sweep.
        let guard = self.write_lock.clone().lock_owned().await;
        let root = self.root.clone();
        let root_handle = Arc::clone(&self.root_handle);
        let content_ref = content_ref.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            refresh_publish_grace_blocking(&root, &root_handle, &content_ref)
        })
        .await
        .map_err(|error| {
            StorageError::driver(StorageCapability::Blob, "refresh_publish_grace", error)
        })?
    }

    fn upload_lease_idle_cap(&self) -> Option<Duration> {
        Some(Duration::from_secs(uploads::lease::MAX_IDLE_SECS))
    }

    async fn begin_upload_with_lease(
        &self,
        size: u64,
        config: UploadLeaseConfig,
    ) -> StorageResult<UploadId> {
        uploads::begin_leased(self, size, config).await
    }

    async fn renew_upload(&self, id: &UploadId) -> StorageResult<()> {
        uploads::renew(self, id.clone()).await
    }

    async fn begin_upload(&self, _declared_size: u64) -> StorageResult<UploadId> {
        Err(StorageError::Unsupported {
            capability: StorageCapability::Blob,
            operation: "begin_upload".into(),
            message: "the filesystem store requires a lease: use begin_upload_with_lease".into(),
        })
    }

    async fn append_part(&self, id: &UploadId, bytes: Vec<u8>) -> StorageResult<u64> {
        uploads::append(self, id.clone(), bytes).await
    }

    async fn commit_upload(&self, id: &UploadId, content_ref: &ContentRef) -> StorageResult<()> {
        uploads::commit(self, id.clone(), content_ref.clone()).await
    }

    async fn abort_upload(&self, id: &UploadId) -> StorageResult<()> {
        uploads::abort(self, id.clone()).await
    }

    async fn sweep_uploads(&self, idle_for: Duration) -> StorageResult<u64> {
        uploads::sweep(self, idle_for).await
    }

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
                put_blocking_from_root_handle(
                    &root,
                    &root_handle,
                    floor_bytes,
                    bytes,
                    #[cfg(unix)]
                    &BlobPublication {
                        #[cfg(test)]
                        hook: hook.as_ref().and_then(|hook| hook.publication.clone()),
                    },
                )
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
#[path = "blob/sync_hook_tests.rs"]
mod sync_hook;

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
#[cfg(all(test, unix))]
#[path = "blob/walk_leaf_sync_hook_tests.rs"]
mod walk_leaf_sync_hook;

/// Test-only pause after a bounded read has opened its authoritative handle
/// and captured handle metadata, but before its first byte read. This makes
/// append/truncate/path-replacement races deterministic without timing sleeps
/// and is deliberately separate from the put/GC lock hook above.
#[cfg(test)]
#[path = "blob/bounded_read_sync_hook_tests.rs"]
mod bounded_read_sync_hook;

/// Test-only pause on `transactional_orphan_sweep`'s database-ownership
/// blocking task, right after the cross-process advisory lock is acquired
/// and before the epoch recheck that immediately follows it. Lets a test
/// mutate the database strictly between the read-only preflight and the
/// recheck, making the recheck's ordering relative to the root guard/lock
/// deterministic instead of racing on scheduling.
#[cfg(test)]
#[path = "blob/db_ownership_sync_hook_tests.rs"]
mod db_ownership_sync_hook;

#[cfg(test)]
#[path = "blob/quarantine_liveness_tests.rs"]
mod quarantine_liveness_tests;

#[cfg(test)]
#[path = "blob_tests.rs"]
mod tests;
