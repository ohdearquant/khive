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
use khive_storage::types::{SqlStatement, SqlValue, StorageResult};
use khive_storage::{AtomicUnitOp, SqlAccess, StorageCapability};

use crate::error::SqliteError;

const ROOT_WRITE_LOCK_FILE: &str = ".khive-blob-write.lock";

fn map_io_err(e: std::io::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Blob, op, e)
}

fn shard_path(root: &Path, content_ref: &ContentRef) -> PathBuf {
    let hex = content_ref.as_str();
    root.join(&hex[0..2]).join(&hex[2..4]).join(hex)
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
    // check and the write entirely.
    if target.exists() {
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
            fs::remove_file(&path).map_err(|e| map_io_err(e, "orphan_sweep_delete"))?;
            result.deleted += 1;
        }
    }
    Ok(result)
}

fn sweep_blob_files(
    root: &Path,
    live_refs: &std::collections::HashSet<ContentRef>,
    dry_run: bool,
    grace_period: Duration,
) -> StorageResult<BlobOrphanSweepResult> {
    let files = walk_blob_files(root).map_err(|e| map_io_err(e, "orphan_sweep_walk"))?;
    sweep_blob_candidates(files, live_refs, dry_run, grace_period)
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
        let path = shard_path(&self.root, content_ref);
        tokio::task::spawn_blocking(move || match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(map_io_err(e, "delete")),
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
    async fn transactional_orphan_sweep(
        &self,
        sql: &dyn SqlAccess,
        dry_run: bool,
    ) -> StorageResult<BlobOrphanSweepResult> {
        let owned_guard = self.write_lock.clone().lock_owned().await;
        let root = self.root.clone();
        let scan_root = root.clone();
        let grace_period = self.orphan_sweep_grace;
        let (write_guards, candidates) = tokio::task::spawn_blocking(move || {
            let root_write_guard = acquire_root_write_lock(&scan_root)?;
            let candidates = walk_blob_files(&scan_root)
                .map_err(|e| map_io_err(e, "transactional_orphan_sweep_walk"))?;
            Ok::<_, StorageError>(((owned_guard, root_write_guard), candidates))
        })
        .await
        .map_err(|e| {
            StorageError::driver(
                StorageCapability::Blob,
                "transactional_orphan_sweep_walk",
                e,
            )
        })??;
        #[cfg(test)]
        let hook = sync_hook::take(&root);
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let _write_guards = write_guards;
                let rows = writer
                    .query_all(SqlStatement {
                        sql: "SELECT DISTINCT content_ref FROM entities \
                              WHERE deleted_at IS NULL AND content_ref IS NOT NULL"
                            .to_string(),
                        params: vec![],
                        label: Some("blob_live_refs".to_string()),
                    })
                    .await?;
                let mut live_refs = std::collections::HashSet::with_capacity(rows.len());
                for row in rows {
                    let raw = match row.get("content_ref") {
                        Some(SqlValue::Text(raw)) => raw,
                        _ => {
                            return Err(StorageError::InvalidInput {
                                capability: StorageCapability::Blob,
                                operation: "transactional_orphan_sweep".into(),
                                message: "entities.content_ref contained a non-text value".into(),
                            });
                        }
                    };
                    let content_ref = ContentRef::from_hex(raw.clone()).map_err(|message| {
                        StorageError::InvalidInput {
                            capability: StorageCapability::Blob,
                            operation: "transactional_orphan_sweep".into(),
                            message,
                        }
                    })?;
                    live_refs.insert(content_ref);
                }
                #[cfg(test)]
                if let Some(hook) = &hook {
                    let _ = hook.reached.send(());
                    let _ = hook.release.recv();
                }
                let result = sweep_blob_candidates(candidates, &live_refs, dry_run, grace_period)?;
                Ok(Box::new(result) as Box<dyn std::any::Any + Send>)
            })
        });
        let result = sql.atomic_unit(op).await?;
        result
            .downcast::<BlobOrphanSweepResult>()
            .map(|result| *result)
            .map_err(|_| {
                StorageError::Internal(
                    "transactional orphan sweep returned an unexpected result type".into(),
                )
            })
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
