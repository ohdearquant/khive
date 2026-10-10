use super::*;

#[cfg(any(test, not(unix)))]
pub(super) fn put_blocking_with_space_probe<F>(
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
pub(super) fn publish_blob_path(source: &Path, target: &Path) -> StorageResult<()> {
    fs::rename(source, target).map_err(|error| map_io_err(error, "put_persist"))
}

#[cfg(any(test, not(unix)))]
pub(super) fn put_blocking(
    root: &Path,
    floor_bytes: u64,
    bytes: Vec<u8>,
) -> StorageResult<ContentRef> {
    let _root_write_guard = acquire_root_write_lock(root)?;
    put_blocking_with_space_probe(root, floor_bytes, bytes, |path| fs4::available_space(path))
}

pub(super) fn acquire_root_write_lock_anchored(
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
pub(super) struct BlobPublication {
    #[cfg(test)]
    pub(super) hook: Option<sync_hook::Publication>,
}

#[cfg(unix)]
impl BlobPublication {
    pub(super) fn step<T>(
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

    pub(super) fn sync_directories(
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

    pub(super) fn sync_directory(
        &self,
        operation: &'static str,
        directory: &fs::File,
    ) -> std::io::Result<()> {
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
pub(super) fn sync_directory(directory: &fs::File) -> std::io::Result<()> {
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
pub(super) fn publish_blob_at(
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
        rename_entry_at(source, temp_name, shard2, content_ref.as_str())
    }) {
        let _ = unlink_entry_at(source.as_raw_fd(), temp_name);
        return Err(error);
    }
    // A barrier failure after rename leaves a complete but unacknowledged
    // object. Do not delete it: readers may already hold its reference.
    publication.sync_directories(root, shard1, shard2)
}

#[cfg(unix)]
pub(super) fn put_blocking_from_root_handle(
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
pub(super) fn put_blocking_from_root_handle(
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
pub(super) fn refresh_publish_grace_blocking(
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
pub(super) fn refresh_publish_grace_blocking(
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
pub(super) fn refresh_publish_grace_blocking(
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
pub(super) fn read_dir_names_no_follow(
    dir_fd: std::os::unix::io::RawFd,
) -> std::io::Result<Vec<String>> {
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
pub(super) fn walk_blob_files_from_root_handle(
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
pub(super) fn walk_blob_files_from_root_handle(
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
pub(super) fn within_publish_grace(
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
