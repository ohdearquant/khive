//! Filesystem-backed `BlobStore` — content-addressed, BLAKE3-sharded on disk.
//!
//! Layout: `<root>/<hex[0..2]>/<hex[2..4]>/<hex>`, a two-level shard identical
//! in shape to git's loose-object store, so a root holding millions of blobs
//! never puts more than a few thousand entries in one directory. Writes are
//! atomic-publish (khive#292): bytes land in a `tempfile` in the SAME shard
//! directory as the final path (guaranteeing same-filesystem rename), the
//! written length is checked against the input length, then
//! `NamedTempFile::persist` performs the rename — crash-safe (a crash mid-write
//! leaves an orphaned temp file, never a partially-committed blob).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use khive_storage::blob::{BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef};
use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::StorageCapability;

use crate::error::SqliteError;

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
/// Pure and filesystem-independent on purpose (round-2 High finding): the
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

fn put_blocking(root: &Path, floor_bytes: u64, bytes: Vec<u8>) -> StorageResult<ContentRef> {
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
    let available = fs4::available_space(root).map_err(|e| map_io_err(e, "put_check_space"))?;
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

/// A `BlobStore` backed by a BLAKE3-sharded directory tree.
pub struct FsBlobStore {
    root: PathBuf,
    floor_bytes: u64,
    /// Serializes the check-then-publish critical section of `put` per root
    /// (round-2 High finding): without this, two concurrent puts can each
    /// observe the same pre-write `available_space` snapshot, each pass
    /// their own write-size-aware floor check against it, and then both
    /// write — jointly pushing the volume under the floor even though
    /// neither write looked unsafe in isolation. Held across the whole
    /// `spawn_blocking` call (check, write, fsync, persist), so the second
    /// of two racing puts always observes the first's write already landed.
    /// A per-root async mutex is adequate at this write rate; it is not
    /// meant to defend against another process (only within-process
    /// `FsBlobStore` callers).
    write_lock: tokio::sync::Mutex<()>,
}

impl FsBlobStore {
    /// Default fail-closed free-space floor (khive#292 SPEC-gate ruling):
    /// 100 GB. Config-overridable via the `floor_bytes` constructor argument.
    pub const DEFAULT_FLOOR_BYTES: u64 = 100_000_000_000;

    /// Create a store rooted at `root`, creating the directory if absent.
    pub fn new(root: PathBuf, floor_bytes: u64) -> Result<Self, SqliteError> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            floor_bytes,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The resolved root directory this store writes under.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        let _write_guard = self.write_lock.lock().await;
        let root = self.root.clone();
        let floor_bytes = self.floor_bytes;
        tokio::task::spawn_blocking(move || put_blocking(&root, floor_bytes, bytes))
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
        tokio::task::spawn_blocking(move || {
            let files = walk_blob_files(&root).map_err(|e| map_io_err(e, "orphan_sweep_walk"))?;
            let mut scanned = 0u64;
            let mut deleted = 0u64;
            let mut would_delete = 0u64;
            for (content_ref, path) in files {
                scanned += 1;
                if live_refs.contains(&content_ref) {
                    continue;
                }
                would_delete += 1;
                if !dry_run {
                    fs::remove_file(&path).map_err(|e| map_io_err(e, "orphan_sweep_delete"))?;
                    deleted += 1;
                }
            }
            Ok(BlobOrphanSweepResult {
                scanned,
                deleted,
                would_delete,
            })
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Blob, "orphan_sweep", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(floor_bytes: u64) -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let store = FsBlobStore::new(root, floor_bytes).unwrap();
        (dir, store)
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
        // Round-2 High, codex's own example verbatim: `available ==
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

    #[tokio::test]
    async fn put_refuses_a_write_that_would_cross_the_floor_even_though_available_alone_clears_it()
    {
        // Integration-level companion to the pure `crosses_floor` tests
        // above: proves `FsBlobStore::put` actually wires the write-size-
        // aware check through end-to-end. Uses a generous margin/cushion
        // (128 MiB total) rather than an exact boundary, because an earlier
        // version of this test pinned the floor to the exact
        // `fs4::available_space` reading and flaked: on this shared,
        // heavily-loaded dev machine, `fs4::available_space` was observed to
        // shift by tens of MB between two calls milliseconds apart (other
        // agents' concurrent builds). A floor-only check would still pass
        // here (available comfortably clears the floor by MARGIN bytes);
        // the write-size-aware check must reject once the payload's own
        // CUSHION-sized excess is subtracted.
        const MARGIN: u64 = 64 * 1024 * 1024;
        const CUSHION: u64 = 64 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir_all(&root).unwrap();
        let available = fs4::available_space(&root).unwrap();
        let floor = available.saturating_sub(MARGIN);
        let store = FsBlobStore::new(root, floor).unwrap();

        let err = store
            .put(vec![7u8; (MARGIN + CUSHION) as usize])
            .await
            .unwrap_err();
        assert!(
            matches!(err, StorageError::CapacityFloor { .. }),
            "a write-size-aware floor check must reject a write that pushes the volume \
             below the floor even though available space alone still clears it: {err:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_puts_cannot_jointly_breach_the_floor_via_a_stale_snapshot() {
        // Round-2 High: pin the floor so at most ONE `payload_len`-sized
        // write may land before the floor is crossed. Fire two DIFFERENT
        // (non-deduping) payloads of that size concurrently. Before the
        // per-root `write_lock`, both puts could independently read the SAME
        // pre-write `available_space` snapshot, both pass their own
        // write-size-aware check against it (neither sees the other's
        // pending write), and both actually write — jointly breaching the
        // floor. With serialization, the second put's check only runs after
        // the first put's write has actually landed on disk, so it observes
        // the reduced space. The decisive, noise-tolerant invariant is that
        // BOTH can never succeed (under the pre-fix code, both reliably
        // would, since the floor is deliberately set a full `payload_len`
        // below the pre-test reading and neither write's own size was
        // subtracted): assert at most one success and at least one
        // rejection, rather than pinning an exact 1-success/1-reject split,
        // since which side of the boundary a given put lands on is
        // legitimately sensitive to real, unrelated disk churn on a shared
        // dev/CI box. `payload_len` (64 MiB) is chosen to dwarf the few-MB
        // swings observed in this environment.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir_all(&root).unwrap();
        let available = fs4::available_space(&root).unwrap();
        let payload_len: u64 = 64 * 1024 * 1024;
        let floor = available.saturating_sub(payload_len);
        let store = std::sync::Arc::new(FsBlobStore::new(root, floor).unwrap());

        let a = {
            let store = store.clone();
            tokio::spawn(async move { store.put(vec![1u8; payload_len as usize]).await })
        };
        let b = {
            let store = store.clone();
            tokio::spawn(async move { store.put(vec![2u8; payload_len as usize]).await })
        };
        let (result_a, result_b) = (a.await.unwrap(), b.await.unwrap());

        let successes = [&result_a, &result_b]
            .into_iter()
            .filter(|r| r.is_ok())
            .count();
        let floor_rejections = [&result_a, &result_b]
            .into_iter()
            .filter(|r| matches!(r, Err(StorageError::CapacityFloor { .. })))
            .count();
        assert!(
            successes <= 1,
            "both concurrent puts landed -- the floor was jointly breached by a stale \
             snapshot race: {result_a:?} / {result_b:?}"
        );
        assert!(
            floor_rejections >= 1,
            "two payload_len writes cannot both fit in a payload_len-sized budget -- at \
             least one rejection is expected: {result_a:?} / {result_b:?}"
        );
    }

    #[tokio::test]
    async fn orphan_sweep_race_demonstrates_the_documented_quiescence_requirement() {
        // Round-2 High: `orphan_sweep` and `delete` are documented
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
