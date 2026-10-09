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
use khive_storage::types::StorageResult;
use khive_storage::{SqlAccess, StorageCapability};

use crate::error::SqliteError;
use uuid::Uuid;

mod gc_locks;
#[cfg(test)]
use gc_locks::database_gc_lock_path;
#[cfg(test)]
pub(crate) use gc_locks::database_gc_waiter_count;
use gc_locks::{acquire_database_gc_lock, sweep_lock_for_database};
pub use gc_locks::{acquire_database_gc_owner, DatabaseGcOwnerGuard};
pub(crate) use gc_locks::{
    acquire_database_gc_owner_for_path_blocking, try_acquire_database_gc_owner_for_path,
};

mod gc_fencing;
use gc_fencing::{
    blob_gc_fence_probe, blob_gc_fencing_complete, claim_blob_gc_batch, parse_blob_gc_claim_rows,
    release_abandoned_blob_gc_claim_batch, release_blob_gc_batch, unsupported_blob_gc_epoch,
    validate_blob_gc_evidence,
};
#[cfg(test)]
use gc_fencing::{blob_gc_fence_probe_with_ids, blob_gc_unowned_attachment_predicate};

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

mod publish;
#[cfg(not(unix))]
use publish::publish_blob_path;
#[cfg(all(test, unix))]
use publish::sync_directory;
use publish::{
    acquire_root_write_lock_anchored, put_blocking_from_root_handle,
    refresh_publish_grace_blocking, walk_blob_files_from_root_handle, within_publish_grace,
};
#[cfg(unix)]
use publish::{publish_blob_at, read_dir_names_no_follow, BlobPublication};
#[cfg(test)]
use publish::{put_blocking, put_blocking_with_space_probe};

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
