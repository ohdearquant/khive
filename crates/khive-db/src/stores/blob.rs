//! Filesystem-backed `BlobStore` — content-addressed, BLAKE3-sharded on disk.
//!
//! Layout: `<root>/<hex[0..2]>/<hex[2..4]>/<hex>`, plus a root-local advisory
//! lock file. The two-level shard is identical in shape to git's loose-object
//! store, so a root holding millions of blobs never puts more than a few
//! thousand entries in one directory. Writes are atomic-publish (khive#292):
//! bytes land in a `tempfile` in the SAME shard directory as the final path
//! (guaranteeing same-filesystem rename), the written length is checked against
//! the input length, then `NamedTempFile::persist` performs the rename —
//! crash-safe (a crash mid-write leaves an orphaned temp file, never a
//! partially-committed blob).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use khive_storage::blob::{BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef};
use khive_storage::error::StorageError;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue, StorageResult};
use khive_storage::{AtomicUnitOp, SqlAccess, StorageCapability};

use crate::error::SqliteError;

const ROOT_WRITE_LOCK_FILE: &str = ".khive-blob-write.lock";
const DATABASE_GC_LOCK_SUFFIX: &str = ".khive-blob-gc.lock";
/// Maximum candidates represented by one claim transaction and its matching
/// physical-delete/cleanup cycle. This bounds JSON binding, returned rows,
/// claim-table pages dirtied per transaction, and exclusive-writer hold time.
const BLOB_GC_CLAIM_BATCH_SIZE: usize = 128;

fn map_io_err(e: std::io::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Blob, op, e)
}

fn shard_path(root: &Path, content_ref: &ContentRef) -> PathBuf {
    let hex = content_ref.as_str();
    root.join(&hex[0..2]).join(&hex[2..4]).join(hex)
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
fn unlink_blob_shard_file_no_follow(root: &Path, content_ref: &ContentRef) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let hex = content_ref.as_str();
    let root_dir = open_dir_no_follow(root)?;
    let shard1_dir = openat_dir_no_follow(root_dir.as_raw_fd(), &hex[0..2])?;
    let shard2_dir = openat_dir_no_follow(shard1_dir.as_raw_fd(), &hex[2..4])?;
    let c_name = std::ffi::CString::new(hex)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `shard2_dir` is a live, open directory descriptor for the
    // duration of this call, and `c_name` is NUL-terminated. `unlinkat`
    // removes the named directory entry only — it acts on the shard
    // directory's entry table, not through any symlink that entry might
    // itself be, so this is safe even if `content_ref`'s target happens to
    // be replaced by a symlink at unlink time.
    let rc = unsafe { libc::unlinkat(shard2_dir.as_raw_fd(), c_name.as_ptr(), 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn unlink_blob_shard_file_no_follow(root: &Path, content_ref: &ContentRef) -> std::io::Result<()> {
    // Windows equivalent of the Unix arm's fd-pinned walk, built from two
    // handle properties instead of `openat`:
    //
    // 1. No-follow verification BY HANDLE: each directory level is opened
    //    with `FILE_FLAG_OPEN_REPARSE_POINT` (plus `FILE_FLAG_BACKUP_SEMANTICS`,
    //    which is what permits opening a directory handle at all), so a
    //    junction or symlink planted at that level yields a handle to the
    //    reparse point itself rather than to its target, and the
    //    handle-derived metadata (`File::metadata`, which queries the handle,
    //    not a re-resolved path) exposes it for refusal.
    // 2. Pinning: the handles are opened WITHOUT `FILE_SHARE_DELETE`. Both
    //    deleting and renaming a directory require an open with `DELETE`
    //    access, and that open fails with a sharing violation while any
    //    handle that did not share delete access is held. Holding all three
    //    verified handles across the final `remove_file` therefore prevents
    //    every checked component from being swapped for a junction in the
    //    check-to-use window the previous `symlink_metadata`-then-delete
    //    shape left open.
    //
    // The final `remove_file` re-resolves `root/<s1>/<s2>/<hex>` through
    // those pinned, verified directories; if the leaf entry itself is a
    // symlink, Windows `DeleteFile` removes the link entry, not its target —
    // the same semantics `unlinkat` gives the Unix arm.
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

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

    let hex = content_ref.as_str();
    let shard1 = root.join(&hex[0..2]);
    let shard2 = shard1.join(&hex[2..4]);
    let _root_pin = open_dir_pinned_no_follow(root)?;
    let _shard1_pin = open_dir_pinned_no_follow(&shard1)?;
    let _shard2_pin = open_dir_pinned_no_follow(&shard2)?;
    fs::remove_file(shard2.join(hex))
}

#[cfg(not(any(unix, windows)))]
fn unlink_blob_shard_file_no_follow(root: &Path, content_ref: &ContentRef) -> std::io::Result<()> {
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
    // out from under the caller's follow-up entity write (khive#1313). The
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

fn put_blocking(root: &Path, floor_bytes: u64, bytes: Vec<u8>) -> StorageResult<ContentRef> {
    let _root_write_guard = acquire_root_write_lock(root)?;
    put_blocking_with_space_probe(root, floor_bytes, bytes, |path| fs4::available_space(path))
}

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

fn walk_blob_files(root: &Path) -> std::io::Result<Vec<(ContentRef, PathBuf)>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for l1 in fs::read_dir(root)? {
        let l1 = l1?;
        if !l1.file_type()?.is_dir() {
            continue;
        }
        for l2 in fs::read_dir(l1.path())? {
            let l2 = l2?;
            if !l2.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(l2.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                // Non-hex names (in-flight `.tmp-*` files from a concurrent
                // `put`, or anything else that landed under `root`) are
                // silently skipped, never swept — orphan_sweep only ever acts
                // on names that already round-trip through `ContentRef`.
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if let Ok(content_ref) = ContentRef::from_hex(name) {
                    out.push((content_ref, entry.path()));
                }
            }
        }
    }
    Ok(out)
}

/// Whether a candidate file is still inside its publish grace period and must
/// be left alone regardless of liveness.
///
/// `put`'s two-step client protocol (bytes land first, a *later* entity write
/// commits the `content_ref`) means a blob can be physically on disk with
/// zero live references for a window entirely outside this store's control —
/// the referencing write simply hasn't happened yet. A file whose mtime is
/// younger than `grace_period` is therefore treated as not-yet-orphaned:
/// `fs::metadata` failing to report an age (removed mid-scan, clock
/// weirdness) is treated the same way (age unknown -> protect it), the safe
/// direction for a sweep that only ever destroys data.
fn within_publish_grace(path: &Path, now: SystemTime, grace_period: Duration) -> bool {
    let age = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok());
    match age {
        Some(age) => age < grace_period,
        None => true,
    }
}

fn sweep_blob_candidates(
    root: &Path,
    files: Vec<(ContentRef, PathBuf)>,
    live_refs: &std::collections::HashSet<ContentRef>,
    dry_run: bool,
    grace_period: Duration,
) -> StorageResult<BlobOrphanSweepResult> {
    let mut result = BlobOrphanSweepResult::default();
    let now = SystemTime::now();
    for (content_ref, path) in files {
        result.scanned += 1;
        if live_refs.contains(&content_ref) {
            continue;
        }
        if within_publish_grace(&path, now, grace_period) {
            result.grace_period_skipped += 1;
            continue;
        }
        result.would_delete += 1;
        if !dry_run {
            unlink_blob_shard_file_no_follow(root, &content_ref)
                .map_err(|e| map_io_err(e, "orphan_sweep_delete"))?;
            result.deleted += 1;
        }
    }
    Ok(result)
}

#[derive(Debug)]
struct PreparedTransactionalSweep {
    result: BlobOrphanSweepResult,
    candidates: Vec<(ContentRef, bool)>,
}

/// Perform every filesystem-dependent part of candidate classification before
/// SQLite's writer transaction opens.
fn prepare_transactional_sweep(
    files: Vec<(ContentRef, PathBuf)>,
    grace_period: Duration,
) -> PreparedTransactionalSweep {
    let now = SystemTime::now();
    let mut result = BlobOrphanSweepResult::default();
    let mut candidates = Vec::with_capacity(files.len());
    for (content_ref, path) in files {
        result.scanned += 1;
        let within_grace = within_publish_grace(&path, now, grace_period);
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

/// Whether this database has ever applied the V20 `blob_gc_claims` migration.
///
/// `transactional_orphan_sweep` is reachable from any `SqlAccess` a caller
/// hands it, including a `StorageBackend` constructed directly (e.g.
/// `StorageBackend::memory()`/`sqlite()` used without `prepare_core_schema`)
/// that never ran core migrations. The claims table and its fencing triggers
/// (`sql/020-blob-gc-claims.sql`) are optional durability/fencing
/// infrastructure, not a hard dependency of the sweep contract itself, so
/// their absence must degrade the sweep rather than fail it outright.
async fn blob_gc_claims_table_exists(sql: &dyn SqlAccess) -> StorageResult<bool> {
    let mut reader = sql.reader().await?;
    let found = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'blob_gc_claims' \
                  LIMIT 1"
                .to_string(),
            params: vec![],
            label: Some("blob_gc_claims_table_exists".to_string()),
        })
        .await?;
    Ok(found.is_some())
}

async fn validate_blob_gc_evidence(
    sql: &dyn SqlAccess,
    has_claims_table: bool,
) -> StorageResult<()> {
    // These full-table integrity probes are statement-scoped reads. Keep them
    // off the single writer; only their one-row result is materialized. The
    // database sweep owner excludes another claim producer, and each bounded
    // claim unit anti-joins the then-current live rows under its writer lock.
    let mut reader = sql.reader().await?;
    if has_claims_table {
        let invalid_claim = reader
            .query_row(SqlStatement {
                sql: "SELECT content_ref FROM blob_gc_claims \
                      WHERE typeof(content_ref) <> 'text' \
                         OR length(content_ref) <> 64 \
                         OR content_ref GLOB '*[^0-9a-f]*' \
                      LIMIT 1"
                    .to_string(),
                params: vec![],
                label: Some("blob_gc_validate_existing_claims".to_string()),
            })
            .await?;
        if invalid_claim.is_some() {
            return Err(invalid_content_ref(
                "blob_gc_claims.content_ref contained a non-canonical value".into(),
            ));
        }
    }

    let invalid_live = reader
        .query_row(SqlStatement {
            sql: "SELECT content_ref FROM entities \
                  WHERE deleted_at IS NULL AND content_ref IS NOT NULL \
                    AND (typeof(content_ref) <> 'text' \
                      OR length(content_ref) <> 64 \
                      OR content_ref GLOB '*[^0-9a-f]*') \
                  LIMIT 1"
                .to_string(),
            params: vec![],
            label: Some("blob_gc_validate_live_refs".to_string()),
        })
        .await?;
    if invalid_live.is_some() {
        return Err(invalid_content_ref(
            "entities.content_ref contained a non-canonical value".into(),
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
    has_claims_table: bool,
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
                                SELECT 1 FROM entities \
                                WHERE deleted_at IS NULL \
                                  AND content_ref = candidate.value \
                              )"
                        .to_string(),
                        params: vec![SqlValue::Text(grace_json)],
                        label: Some("blob_gc_count_grace_candidates_batch".to_string()),
                    })
                    .await?,
                "blob_gc_count_grace_candidates_batch",
            )?;

            // No `blob_gc_claims` table (a direct `StorageBackend` that never
            // applied the V20 migration): there is nothing to durably claim
            // and no entity-trigger fence to rely on, so this degrades to
            // the same snapshot-then-delete guarantee `orphan_sweep`
            // documents — the anti-join is evaluated here, one bounded batch
            // at a time, but a reference committed live between this read
            // and the physical delete below is not protected against.
            if !has_claims_table {
                let eligible_rows = writer
                    .query_all(SqlStatement {
                        sql: "SELECT candidate.value AS content_ref \
                              FROM json_each(?1) AS candidate \
                              WHERE NOT EXISTS ( \
                                SELECT 1 FROM entities \
                                WHERE deleted_at IS NULL \
                                  AND content_ref = candidate.value \
                              ) ORDER BY candidate.value"
                            .to_string(),
                        params: vec![SqlValue::Text(eligible_json)],
                        label: Some(
                            "blob_gc_select_eligible_candidates_batch_no_claims_table".to_string(),
                        ),
                    })
                    .await?;
                let would_delete = eligible_rows.len() as u64;
                return Ok(Box::new(BlobGcBatchRows {
                    grace_period_skipped,
                    would_delete,
                    claimed_rows: if dry_run { Vec::new() } else { eligible_rows },
                }) as Box<dyn std::any::Any + Send>);
            }

            if dry_run {
                let would_delete = required_nonnegative_count(
                    writer
                        .query_scalar(SqlStatement {
                            sql: "SELECT COUNT(*) FROM json_each(?1) AS candidate \
                                  WHERE NOT EXISTS ( \
                                    SELECT 1 FROM entities \
                                    WHERE deleted_at IS NULL \
                                      AND content_ref = candidate.value \
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
                            SELECT 1 FROM entities \
                            WHERE deleted_at IS NULL \
                              AND content_ref = candidate.value \
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

fn sweep_blob_files(
    root: &Path,
    live_refs: &std::collections::HashSet<ContentRef>,
    dry_run: bool,
    grace_period: Duration,
) -> StorageResult<BlobOrphanSweepResult> {
    let files = walk_blob_files(root).map_err(|e| map_io_err(e, "orphan_sweep_walk"))?;
    sweep_blob_candidates(root, files, live_refs, dry_run, grace_period)
}

/// Process-wide database owner fence for transactional blob sweeps.
///
/// Claims live in the database and their entity triggers are database-global,
/// so a root-only lock is insufficient: two differently configured roots for
/// one database must not recover each other's live claims. File-backed pools
/// additionally take [`acquire_database_gc_lock`] for cross-process exclusion.
type SweepLockMap = HashMap<Option<PathBuf>, Arc<tokio::sync::Mutex<()>>>;

fn database_sweep_locks() -> &'static StdMutex<SweepLockMap> {
    static REGISTRY: OnceLock<StdMutex<SweepLockMap>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn sweep_lock_for_database(database_path: Option<&Path>) -> Arc<tokio::sync::Mutex<()>> {
    let key = database_path.map(Path::to_path_buf);
    let mut locks = database_sweep_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
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
    /// separate entity write that commits a `content_ref` to it; it does not
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
    /// call returning and its follow-up entity write landing, not any
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
        let metadata = fs::metadata(&root)?;
        if !metadata.is_dir() {
            return Err(SqliteError::InvalidData(format!(
                "blob store root is not a directory: {}",
                root.display()
            )));
        }
        let write_lock = write_lock_for_root(&root)?;
        Ok(Self {
            root,
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
                put_blocking(&root, floor_bytes, bytes)
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

    async fn get(&self, content_ref: &ContentRef) -> StorageResult<Vec<u8>> {
        let path = shard_path(&self.root, content_ref);
        let key = content_ref.to_string();
        tokio::task::spawn_blocking(move || {
            fs::read(&path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    StorageError::NotFound {
                        capability: StorageCapability::Blob,
                        resource: "blob",
                        key,
                    }
                } else {
                    map_io_err(e, "get")
                }
            })
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "get", e))?
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let path = shard_path(&self.root, content_ref);
        tokio::task::spawn_blocking(move || Ok(path.exists()))
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Blob, "exists", e))?
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        let path = shard_path(&self.root, content_ref);
        tokio::task::spawn_blocking(move || match fs::metadata(&path) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(map_io_err(e, "size")),
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "size", e))?
    }

    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let root = self.root.clone();
        let content_ref = content_ref.clone();
        tokio::task::spawn_blocking(move || {
            match unlink_blob_shard_file_no_follow(&root, &content_ref) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(map_io_err(e, "delete")),
            }
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "delete", e))?
    }

    // Offline-maintenance-only — see `BlobStore::orphan_sweep`'s doc comment
    // for the concurrency hazard (`config.live_refs` is a snapshot; a
    // `content_ref` that becomes live after the snapshot is deleted anyway).
    // This method performs no DB coordination; it only compares against
    // whatever set the caller handed it.
    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> {
        let root = self.root.clone();
        let live_refs = config.live_refs.clone();
        let dry_run = config.dry_run;
        let grace_period = self.orphan_sweep_grace;
        tokio::task::spawn_blocking(move || {
            sweep_blob_files(&root, &live_refs, dry_run, grace_period)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "orphan_sweep", e))?
    }

    // `put` and the entity write that later commits a `content_ref` to its
    // result are two separate steps of the client protocol -- the write
    // lock this method takes only serializes it against a concurrent `put`,
    // it is not held across the caller's own gap between finishing `put` and
    // issuing that follow-up entity write. A blob can therefore be fully on
    // disk with zero live references purely because its referencing write
    // hasn't landed yet, not because it is actually orphaned.
    // `within_publish_grace` (via `orphan_sweep_grace`) is what protects that
    // window: a file younger than the grace period is left alone regardless
    // of liveness. Residual assumption: a client that waits longer than the
    // grace period between `put` returning and its entity write committing
    // is still exposed to this method deleting the blob out from under it --
    // callers with an unusually slow publish path should widen the grace
    // period (`FsBlobStore::with_orphan_sweep_grace`) accordingly.
    //
    // Cross-resource ordering (#1850): database/root ownership and filesystem
    // walk/metadata happen before SQL. Bounded SQL-only units recover abandoned
    // rows and commit at most 128 fresh claims whose entity triggers fence new
    // live references; physical deletion happens after each COMMIT; a second
    // bounded SQL-only unit releases that batch. Owner/root locks span all
    // phases, but SQLite's single writer never spans external I/O.
    async fn transactional_orphan_sweep(
        &self,
        sql: &dyn SqlAccess,
        dry_run: bool,
    ) -> StorageResult<BlobOrphanSweepResult> {
        // Claims and their entity triggers are database-global. Serialize the
        // whole cross-resource protocol by database before taking the root
        // locks, so differently configured roots cannot recover one another's
        // active claim batches. The OS lock is the crash-detecting owner:
        // acquiring it proves that every row left in this database is
        // abandoned, including rows copied by backup or left before a root
        // relocation.
        let database_path = sql.database_path();
        let database_guard = sweep_lock_for_database(database_path.as_deref())
            .lock_owned()
            .await;
        let root_guard = self.write_lock.clone().lock_owned().await;
        let root = self.root.clone();
        let scan_root = root.clone();
        let lock_database_path = database_path.clone();
        let grace_period = self.orphan_sweep_grace;
        let (write_guards, canonical_root, prepared) = tokio::task::spawn_blocking(move || {
            let database_file_guard = acquire_database_gc_lock(lock_database_path.as_deref())?;
            let canonical_root = scan_root
                .canonicalize()
                .map_err(|e| map_io_err(e, "transactional_orphan_sweep_root"))?;
            let root_write_guard = acquire_root_write_lock(&canonical_root)?;
            let candidates = walk_blob_files(&canonical_root)
                .map_err(|e| map_io_err(e, "transactional_orphan_sweep_walk"))?;
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
        let has_claims_table = blob_gc_claims_table_exists(sql).await?;
        validate_blob_gc_evidence(sql, has_claims_table).await?;
        if !dry_run && has_claims_table {
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
            let batch =
                claim_blob_gc_batch(sql, root_key.clone(), candidates, dry_run, has_claims_table)
                    .await?;
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
                        match unlink_blob_shard_file_no_follow(&delete_root, &content_ref) {
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
            // reevaluates them rather than resuming deletion blindly. No
            // claims table means nothing was inserted to release.
            if has_claims_table {
                release_blob_gc_batch(sql, root_key.clone()).await?;
            }
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

        let error = unlink_blob_shard_file_no_follow(&root, &fake_ref).unwrap_err();
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
        // sweep normally after the fix.
        let swept = store
            .orphan_sweep(&BlobOrphanSweepConfig {
                live_refs: std::collections::HashSet::new(),
                dry_run: false,
            })
            .await
            .unwrap();
        assert_eq!(
            swept.deleted, 1,
            "the real, unsymlinked shard must still sweep"
        );
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
    async fn put_get_roundtrip() {
        let (_dir, store) = store(0);
        let bytes = b"hello blob store".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        let fetched = store.get(&content_ref).await.unwrap();
        assert_eq!(fetched, bytes);
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
        assert_eq!(store.get(&first).await.unwrap(), bytes);
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
    async fn get_missing_content_ref_returns_not_found() {
        let (_dir, store) = store(0);
        let missing = ContentRef::from_hex("e".repeat(64)).unwrap();
        let err = store.get(&missing).await.unwrap_err();
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

    #[tokio::test]
    async fn orphan_sweep_race_demonstrates_the_documented_quiescence_requirement() {
        // `orphan_sweep` and `delete` are documented
        // (`BlobStore::orphan_sweep`'s doc comment, ADR-111 §8) as
        // offline-maintenance-only APIs that require the caller to quiesce
        // entity writes for the duration of snapshot-plus-sweep, because
        // `live_refs` is a snapshot with no database coordination. This test
        // reproduces the exact hazard in code rather than leaving it as
        // prose: a blob that becomes newly "live" AFTER the caller's
        // `live_refs` snapshot was taken, but BEFORE the sweep runs, is
        // deleted anyway. That is the documented boundary, not a bug in this
        // test — it exists so a future change that silently narrows this
        // hazard (without updating the docs) breaks a test instead of
        // shipping a doc/behavior mismatch.
        let (_dir, store) = store(0);
        let blob = store
            .put(b"about to become live mid-sweep".to_vec())
            .await
            .unwrap();

        // The caller's live_refs snapshot was taken before an entity write
        // referencing `blob` landed (represented here by simply never adding
        // it to the snapshot — orphan_sweep has no other way to learn about
        // it).
        let live_refs_snapshot = std::collections::HashSet::new();

        let result = store
            .orphan_sweep(&BlobOrphanSweepConfig {
                live_refs: live_refs_snapshot,
                dry_run: false,
            })
            .await
            .unwrap();

        assert_eq!(
            result.deleted, 1,
            "the now-live blob is deleted anyway: this is the documented hazard"
        );
        assert!(
            !store.exists(&blob).await.unwrap(),
            "orphan_sweep is unsafe against a content_ref that becomes live after the \
             snapshot was taken — callers MUST quiesce entity writes before running it \
             (ADR-111 §8)"
        );
    }

    /// A `StorageBackend` constructed directly and never run through the
    /// versioned migration ledger (`run_migrations`/`prepare_core_schema`) —
    /// only the ad hoc, idempotent `entities` DDL a plain `entities()` call
    /// applies, exactly the "tests that create stores directly" pattern
    /// `backend.rs`'s own `prepare_core_schema` doc comment names — must
    /// still be able to run `transactional_orphan_sweep`. `blob_gc_claims`
    /// is V20, migration-only DDL; it must not exist on this backend, and
    /// the sweep must not require it to.
    #[tokio::test]
    async fn transactional_orphan_sweep_works_without_the_blob_gc_claims_migration() {
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
        let result = store
            .transactional_orphan_sweep(sql.as_ref(), false)
            .await
            .expect("sweep must succeed on a direct backend without the blob_gc_claims migration");
        assert_eq!(result.deleted, 1);
        assert!(!store.exists(&orphan).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_preserves_put_started_after_liveness_mark() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            crate::run_migrations(writer.conn_mut()).unwrap();
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
        // selected for deletion, a concurrent entity writer cannot make it
        // newly live in the released-writer window.
        let claimed_err = unrelated
            .execute(
                "INSERT INTO entities \
                 (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
                 VALUES ('racing-reference', 'local', 'document', 'racing', '[]', 1, 1, ?1)",
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES (?1, ?2, 1), (?1, ?3, 1)",
                    rusqlite::params![root_key, content_ref.as_str(), absent_ref],
                )
                .unwrap();
        }

        // A publisher that recovered after the claiming process crashed
        // refreshes the digest's grace witness before its entity write. The
        // next sweep must clear both this protected claim and the claim whose
        // file was already removed, never resume deletion blindly.
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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
        conn.execute(
            "INSERT INTO entities \
             (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
             VALUES ('corrupt-live', 'local', 'document', 'corrupt', '[]', 1, 1, \
                     'not-a-content-ref')",
            [],
        )
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

        conn.execute("DELETE FROM entities WHERE id = 'corrupt-live'", [])
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transactional_orphan_sweep_republishes_deduplicated_external_put() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = std::sync::Arc::new(crate::StorageBackend::sqlite(&db_path).unwrap());
        {
            let mut writer = backend.pool().writer().unwrap();
            crate::run_migrations(writer.conn_mut()).unwrap();
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
    async fn transactional_orphan_sweep_uses_only_non_deleted_entity_refs_as_live() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            crate::run_migrations(writer.conn_mut()).unwrap();
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
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at, content_ref) \
                     VALUES ('live', 'local', 'document', 'live', '[]', 1, 1, NULL, ?1), \
                            ('deleted', 'local', 'document', 'deleted', '[]', 1, 1, 2, ?2)",
                    rusqlite::params![live.as_str(), soft_deleted.as_str()],
                )
                .unwrap();
        }

        let dry_run = store
            .transactional_orphan_sweep(backend.sql().as_ref(), true)
            .await
            .unwrap();
        assert_eq!(dry_run.would_delete, 2);
        assert_eq!(dry_run.deleted, 0);
        assert!(store.exists(&soft_deleted).await.unwrap());
        assert!(store.exists(&orphan).await.unwrap());

        let result = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .unwrap();

        assert_eq!(result.scanned, 3);
        assert_eq!(result.deleted, 2);
        assert!(store.exists(&live).await.unwrap());
        assert!(!store.exists(&soft_deleted).await.unwrap());
        assert!(!store.exists(&orphan).await.unwrap());
    }

    #[tokio::test]
    async fn transactional_orphan_sweep_protects_a_freshly_published_blob_before_its_reference_commits(
    ) {
        // The exact two-step client protocol hazard: `put` completes and
        // releases its write lock (step 1) while the entity write that will
        // *later* commit a `content_ref` to this blob (step 2) has not
        // happened yet -- nothing in this store's locking serializes the
        // two, because they are separate calls the client makes with an
        // arbitrary gap in between. A sweep that lands in that gap must not
        // delete the blob: `entities.content_ref` has no row for it yet
        // purely because the referencing write hasn't landed, not because
        // it is actually orphaned. Without the publish-grace window this
        // reproduces khive#1313's dangling-reference defect: the blob file
        // is deleted here, and the still-pending entity write below would
        // commit a `content_ref` to nothing.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            crate::run_migrations(writer.conn_mut()).unwrap();
        }
        // Default (non-zero) grace period -- this test exercises exactly
        // what it exists to protect.
        let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();

        // Step 1: put completes, lock released. No entity anywhere
        // references this blob yet.
        let blob = store
            .put(b"published, reference not yet committed".to_vec())
            .await
            .unwrap();

        // A sweep runs in the gap before step 2 (the entity write) happens.
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

        // Step 2 now lands: the entity write commits content_ref to the
        // still-present blob.
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at, content_ref) \
                     VALUES ('e1', 'local', 'document', 'e1', '[]', 1, 1, NULL, ?1)",
                    rusqlite::params![blob.as_str()],
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
        // gap before the caller's follow-up entity write could delete it
        // out from under that write (khive#1313). This reproduces the
        // race end to end and proves the mtime refresh closes it.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            crate::run_migrations(writer.conn_mut()).unwrap();
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

        // A deduplicating put republishes the identical bytes. No entity
        // anywhere references this blob yet.
        let second = store.put(bytes).await.unwrap();
        assert_eq!(first, second);

        // The sweep lands in the gap before the follow-up entity write --
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

        // The caller's follow-up entity write now lands.
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at, content_ref) \
                     VALUES ('e1', 'local', 'document', 'e1', '[]', 1, 1, NULL, ?1)",
                    rusqlite::params![first.as_str()],
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
        // The assumption the fix relies on for every `store(0)`-flavored
        // test in this file: `within_publish_grace` with `Duration::ZERO`
        // never protects a candidate regardless of its mtime (`age <
        // Duration::ZERO` is always false), so refreshing the mtime on a
        // deduplicated republish must not change zero-grace sweep behavior.
        // Verified directly rather than assumed.
        let (_dir, store) = store(0);
        let bytes = b"zero grace dedup refresh".to_vec();
        let first = store.put(bytes.clone()).await.unwrap();
        let second = store.put(bytes.clone()).await.unwrap();
        assert_eq!(first, second);

        let result = store
            .orphan_sweep(&BlobOrphanSweepConfig {
                live_refs: std::collections::HashSet::new(),
                dry_run: false,
            })
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
            crate::run_migrations(writer.conn_mut()).unwrap();
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

    #[tokio::test]
    async fn orphan_sweep_dry_run_reports_without_deleting() {
        let (_dir, store) = store(0);
        let live = store.put(b"keep me".to_vec()).await.unwrap();
        let orphan = store.put(b"orphaned".to_vec()).await.unwrap();

        let mut live_refs = std::collections::HashSet::new();
        live_refs.insert(live.clone());
        let result = store
            .orphan_sweep(&BlobOrphanSweepConfig {
                live_refs,
                dry_run: true,
            })
            .await
            .unwrap();

        assert_eq!(result.scanned, 2);
        assert_eq!(result.would_delete, 1);
        assert_eq!(result.deleted, 0);
        assert!(
            store.exists(&orphan).await.unwrap(),
            "dry run must not delete"
        );
        assert!(store.exists(&live).await.unwrap());
    }

    #[tokio::test]
    async fn orphan_sweep_real_run_deletes_only_unreferenced_blobs() {
        let (_dir, store) = store(0);
        let live = store.put(b"keep me".to_vec()).await.unwrap();
        let orphan = store.put(b"orphaned".to_vec()).await.unwrap();

        let mut live_refs = std::collections::HashSet::new();
        live_refs.insert(live.clone());
        let result = store
            .orphan_sweep(&BlobOrphanSweepConfig {
                live_refs,
                dry_run: false,
            })
            .await
            .unwrap();

        assert_eq!(result.scanned, 2);
        assert_eq!(result.would_delete, 1);
        assert_eq!(result.deleted, 1);
        assert!(!store.exists(&orphan).await.unwrap());
        assert!(
            store.exists(&live).await.unwrap(),
            "live blob must survive sweep"
        );
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
}
