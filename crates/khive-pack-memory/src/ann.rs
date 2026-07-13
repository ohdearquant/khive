//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_runtime::{
    is_benign_shutdown_cancellation, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::StorageError;
use khive_vamana::{CorpusFingerprint, VamanaConfig, VamanaIndex, VamanaSnapshot};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

// ── types ─────────────────────────────────────────────────────────────────────

/// Cache key for a per-model ANN slot (one index per model, all namespaces combined).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct AnnKey {
    pub(crate) model: String,
}

impl AnnKey {
    pub(crate) fn new(_namespace: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }

    pub(crate) fn from_token(_token: &NamespaceToken, model: &str) -> Self {
        Self {
            model: model.to_owned(),
        }
    }
}

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    id_map: Vec<Uuid>,
    /// Distinct namespaces present in the indexed corpus.
    /// Used by the recall retry gate to short-circuit when the global index
    /// contains no vectors outside the caller's visible namespace set.
    pub(crate) namespace_set: HashSet<String>,
    /// Write-generation this build's corpus snapshot was taken at or after
    /// (#750). Captured from `AnnState::generations` BEFORE the corpus scan
    /// begins — see `ensure_ann_for_model_inner`. Cache installation compares
    /// this against the currently-installed entry's generation instead of
    /// blindly `or_insert`-ing, so a build that snapshotted a stale (older)
    /// corpus can never clobber an already-installed fresher build, and a
    /// build that snapshotted a fresher corpus always wins.
    pub(crate) generation: u64,
    /// Durable corpus epoch (`memory_ann_epoch` table) observed at build/load
    /// time (#812). Distinct from `generation`,
    /// which lives only in this process's memory and resets to 0 on restart
    /// — `epoch_baseline` is compared against a value written to the shared
    /// SQLite file, so it is the only signal a warm daemon has for an
    /// out-of-process corpus mutation (`kkernel reindex`) that never goes
    /// through this daemon's `bump_generation` call at all.
    pub(crate) epoch_baseline: u64,
}

/// Shared ANN state: per-`(namespace, model)` indexes with at-most-one-background-build guard.
pub(crate) struct AnnState {
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    /// Plain `std::sync::Mutex`, not `tokio::sync::Mutex`: every critical
    /// section here is a synchronous check-and-mutate with no `.await`
    /// inside it, which lets `WarmingGuard::drop` (below) release the guard
    /// synchronously on every exit path of the background task it's tied to,
    /// including success, error, or panic (#812), instead of needing
    /// to spawn a second task just to await a `tokio::sync::Mutex`.
    warming: std::sync::Mutex<HashSet<AnnKey>>,
    /// Issue #723: per-model single-flight lock
    /// owned by `ensure_ann_for_model` itself, the chokepoint every warm
    /// path (boot warm, background fire-once warm, recall-miss warm) routes
    /// through. Distinct from `warming` above, which is `ensure_ann_background`'s
    /// own fire-once-and-forget guard against re-spawning a background task —
    /// this map instead lets a second concurrent caller actually wait for the
    /// in-flight attempt to finish, so only one caller ever emits the
    /// `PhaseStarted`/`PhaseCompleted` pair for a given model.
    model_locks: Mutex<HashMap<AnnKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Monotonic per-model write-generation counter (#750). Bumped by
    /// `bump_generation` whenever a write may have changed a model's corpus
    /// (`memory.remember`'s affected-models loop, right where the old
    /// `invalidate_namespace` clear already ran). `ensure_ann_for_model`
    /// snapshots the current value for its model BEFORE doing anything else
    /// — including before its own "already loaded" fast path and before the
    /// corpus scan — and stamps it on the resulting `AnnBridge`. Cache
    /// install then only replaces an existing entry when the candidate's
    /// generation is >= the installed entry's, instead of the old
    /// `entry(key).or_insert(...)`, which always kept whichever build
    /// happened to acquire the per-model lock first even if it had
    /// snapshotted a now-stale corpus.
    generations: Mutex<HashMap<AnnKey, u64>>,
    /// Debounce state for `maybe_check_durable_epoch` (#812): last time this
    /// process actually paid for a
    /// `memory_ann_epoch` SELECT for a given key, so the warm-hit recall
    /// path amortizes the cross-process freshness check instead of adding a
    /// DB round-trip to every single recall.
    last_epoch_check: std::sync::Mutex<HashMap<AnnKey, std::time::Instant>>,
    /// Counts how many times `search_loaded` returned a warm hit. Test-only;
    /// call `reset_warm_route_count()` between operations to isolate counts.
    #[cfg(test)]
    pub(crate) warm_route_count: AtomicUsize,
    /// Test-only synchronization point: notified right before each rebuild
    /// attempt inside `ensure_ann_background`'s tracked task commits to the
    /// generation floor it captured (#812).
    /// Scoped per-`AnnState` (not a process-global static) so one test's
    /// notifications can never be consumed by a different test's waiter —
    /// each test gets its own via `new_shared()`.
    #[cfg(test)]
    pub(crate) attempt_floor_notify: tokio::sync::Notify,
    /// Test-only reverse barrier (#812): the
    /// `Notify` test still lacked an ordering guarantee): when
    /// `attempt_floor_barrier` is armed, the tracked task's first attempt
    /// WAITS on this after emitting `attempt_floor_notify`, so a test can
    /// deterministically bump the generation before the task's build is
    /// allowed to proceed, instead of racing the build's own completion.
    #[cfg(test)]
    pub(crate) attempt_floor_release: tokio::sync::Notify,
    /// Test-only: arms the two-way handshake above. Defaults to `false` so
    /// every other test driving `ensure_ann_background`/`spawn_rebuild_task`
    /// (which never call `attempt_floor_release.notify_one()`) is unaffected
    /// — only a test that explicitly sets this waits on the release signal.
    #[cfg(test)]
    pub(crate) attempt_floor_barrier: std::sync::atomic::AtomicBool,
    /// Test-only completion signal: notified whenever `WarmingGuard::drop`
    /// releases `key` from `warming` (#844). A test that fires a warm-up
    /// recall has no other way to observe that the background rebuild the
    /// recall (or a preceding `memory.remember`) triggered has actually
    /// finished — `ensure_ann_background`'s only return value is whether it
    /// started a task, and `track_background_task` itself hands back no
    /// `JoinHandle` (it is deliberately fire-and-forget in production, see
    /// its doc comment). `wait_until_warm_idle` below pairs with this to give
    /// tests a deterministic barrier instead of polling on a sleep loop.
    #[cfg(test)]
    pub(crate) warming_idle: tokio::sync::Notify,
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(AnnState {
        indexes: RwLock::new(HashMap::new()),
        warming: std::sync::Mutex::new(HashSet::new()),
        model_locks: Mutex::new(HashMap::new()),
        generations: Mutex::new(HashMap::new()),
        last_epoch_check: std::sync::Mutex::new(HashMap::new()),
        #[cfg(test)]
        warm_route_count: AtomicUsize::new(0),
        #[cfg(test)]
        attempt_floor_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        attempt_floor_release: tokio::sync::Notify::new(),
        #[cfg(test)]
        attempt_floor_barrier: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        warming_idle: tokio::sync::Notify::new(),
    })
}

/// Bump `key`'s write-generation counter and return the NEW value (#750).
/// Called by every memory write path (`memory.remember`, `memory.prune`,
/// and the KG-side note-mutation hook in `pack.rs`) for every model the
/// write may have affected.
///
/// #791: this is now the ONLY invalidation signal a write emits — it no
/// longer clears the in-memory index or deletes the persisted snapshot.
/// The stale-but-installed entry stays put and keeps serving reads
/// (`ann::search_loaded` doesn't consult freshness at all;
/// `handlers/common.rs`'s recall path serves it and fires the
/// already-existing `ensure_ann_background` warm rather than blocking the
/// request on a synchronous rebuild). `install_if_fresher` guarantees the
/// stale entry is replaced, never merged or corrupted, once a build that
/// snapshotted at or after this generation installs.
pub(crate) async fn bump_generation(ann: &SharedAnn, key: &AnnKey) -> u64 {
    let mut gens = ann.generations.lock().await;
    let slot = gens.entry(key.clone()).or_insert(0);
    *slot += 1;
    *slot
}

/// Read `key`'s current write-generation counter (0 if never bumped).
async fn current_generation(ann: &SharedAnn, key: &AnnKey) -> u64 {
    ann.generations.lock().await.get(key).copied().unwrap_or(0)
}

/// True when the currently-installed entry for `key` (if any) is fresh
/// enough to satisfy a caller whose write-generation floor is
/// `min_generation` — i.e. the installed build's own generation is >= what
/// the caller needs. `Ok(None)`-style "cache miss" callers should treat
/// `false` as "must (re)build", not merely "absent".
async fn installed_is_fresh(ann: &SharedAnn, key: &AnnKey, min_generation: u64) -> bool {
    ann.indexes
        .read()
        .await
        .get(key)
        .is_some_and(|b| b.generation >= min_generation)
}

/// True when the currently-cached entry for `key` (if any) reflects at
/// least `key`'s latest recorded write generation (#750). Recall's
/// cache-hit gate uses this instead of a bare presence check, so a stale
/// entry left behind by a slow, superseded background build (one that lost
/// the freshness race but still reached an empty cache slot first) is
/// treated the same as a genuine cache miss — forcing the same
/// `ensure_ann_for_model` fallback a true miss would take, instead of
/// silently serving results that predate a write the caller can already
/// see committed.
pub(crate) async fn is_current(ann: &SharedAnn, key: &AnnKey) -> bool {
    let target_generation = current_generation(ann, key).await;
    installed_is_fresh(ann, key, target_generation).await
}

/// How often `maybe_check_durable_epoch` is willing to pay for a
/// `memory_ann_epoch` SELECT for the same key (#812). A warm-hit recall calls
/// this on every request; without debouncing
/// that would add a synchronous DB round-trip to the hot path this whole PR
/// exists to keep off. Zero in tests so a single direct call always performs
/// the check deterministically instead of racing a wall-clock interval.
#[cfg(not(test))]
const DURABLE_EPOCH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const DURABLE_EPOCH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(0);

/// Minimum delay before a CHAINED rebuild task's first attempt — i.e. a task
/// spawned by `spawn_rebuild_task`'s own post-release re-enqueue, not the
/// original caller-triggered spawn (#812). Without this,
/// a corpus that receives a new write on every rebuild attempt chains a
/// fresh `ATTEMPT_BOUND`-attempt task immediately after the previous one
/// exits, forever, spending unbounded aggregate CPU under continuous
/// writes. Sleeping here — between chained spawns, not inside a single
/// task's attempt loop — lets writes that land during the delay coalesce
/// into the next chained task's single generation read instead of each one
/// re-triggering its own chain link.
#[cfg(not(test))]
const REBUILD_CHAIN_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
const REBUILD_CHAIN_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(5);

/// Amortized cross-process freshness check (#812): compares the durable
/// `memory_ann_epoch` row against the
/// currently-installed entry's `epoch_baseline` for `key`, and — if the
/// durable epoch has moved on — bumps `key`'s in-memory write-generation so
/// the existing generation machinery (`is_current`, `ensure_ann_background`'s
/// single-flight rebuild) takes over exactly as it would for an in-process
/// write.
///
/// This exists because `kkernel reindex` runs as a separate OS process: it
/// mutates the vector table and deletes the persisted snapshot row directly,
/// never touching this daemon's in-memory `generations` map, so an
/// already-warm daemon's `common.rs` recall path (`ann::is_current`) had no
/// way to ever notice — the daemon would serve pre-reindex vectors
/// indefinitely. `bump_memory_ann_epoch` (called from `kkernel::reindex`
/// after it invalidates the snapshot) is the durable side of this signal.
pub(crate) async fn maybe_check_durable_epoch(rt: &KhiveRuntime, ann: &SharedAnn, key: &AnnKey) {
    {
        let mut last = ann
            .last_epoch_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let due = last
            .get(key)
            .is_none_or(|t| now.duration_since(*t) >= DURABLE_EPOCH_CHECK_INTERVAL);
        if !due {
            return;
        }
        last.insert(key.clone(), now);
    }

    let installed_baseline = ann.indexes.read().await.get(key).map(|b| b.epoch_baseline);
    let Some(baseline) = installed_baseline else {
        // Nothing installed yet — a genuine cache miss already routes through
        // the normal build path, which reads the durable epoch itself.
        return;
    };
    let durable = durable_epoch(rt).await;
    if durable > baseline {
        tracing::debug!(
            model = %key.model,
            baseline,
            durable,
            "memory ANN durable epoch advanced; marking cached entry stale"
        );
        bump_generation(ann, key).await;
    }
}

/// DDL for the durable memory-ANN corpus epoch table (#812). Declared here so
/// `MemoryPack::SCHEMA_PLAN` (`pack.rs`) can own it like every other
/// pack-auxiliary table in this codebase (ADR-028), instead of the table
/// being created ad hoc, inline, on the epoch bump/read path.
pub(crate) const MEMORY_SCHEMA_PLAN_STMTS: [&str; 1] =
    ["CREATE TABLE IF NOT EXISTS memory_ann_epoch (\
     id INTEGER PRIMARY KEY CHECK (id = 1), \
     epoch INTEGER NOT NULL DEFAULT 0\
 )"];

/// Ensure the `memory_ann_epoch` table exists (idempotent). NOT called from
/// the epoch bump/read hot path anymore (#812). A daemon boot applies
/// `MemoryPack::SCHEMA_PLAN` up front
/// (`server.rs`/`serve.rs`), and `kkernel reindex` (which runs directly
/// against a raw `KhiveRuntime`, never booting a pack registry) calls this
/// explicitly, once, via `khive_pack_memory::ensure_ann_epoch_schema` before
/// its first durable-epoch bump.
pub(crate) async fn ensure_epoch_schema(rt: &KhiveRuntime) -> Result<(), RuntimeError> {
    let sql = rt.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    w.execute_script(MEMORY_SCHEMA_PLAN_STMTS[0].to_string())
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))
}

/// Read the durable corpus epoch (0 if the table doesn't exist yet or the
/// row is absent — the same "nothing has ever bumped this" baseline every
/// `AnnBridge` implicitly starts from via `epoch_baseline: 0`).
pub(crate) async fn durable_epoch(rt: &KhiveRuntime) -> u64 {
    let sql = rt.sql();
    let Ok(mut reader) = sql.reader().await else {
        return 0;
    };
    let Ok(rows) = reader
        .query_all(SqlStatement {
            sql: "SELECT epoch FROM memory_ann_epoch WHERE id = 1".into(),
            params: vec![],
            label: Some("memory_ann_durable_epoch_read".into()),
        })
        .await
    else {
        return 0;
    };
    match rows.first().and_then(|r| r.get("epoch")) {
        Some(SqlValue::Integer(n)) if *n >= 0 => *n as u64,
        _ => 0,
    }
}

/// Bump the durable corpus epoch by one and return the new value. Called by
/// `kkernel reindex` (via `khive_pack_memory::bump_memory_ann_epoch`) after
/// it invalidates the persisted snapshot, so an already-warm daemon sharing
/// the same database file has a durable signal to observe (#812).
pub(crate) async fn bump_durable_epoch(rt: &KhiveRuntime) -> Result<u64, RuntimeError> {
    let sql = rt.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    w.execute(SqlStatement {
        sql: "INSERT INTO memory_ann_epoch (id, epoch) VALUES (1, 1) \
              ON CONFLICT(id) DO UPDATE SET epoch = epoch + 1"
            .into(),
        params: vec![],
        label: Some("memory_ann_durable_epoch_bump".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    drop(w);
    Ok(durable_epoch(rt).await)
}

/// Install `candidate` into the cache for `key` UNLESS an entry is already
/// present with a generation >= `candidate.generation` (#750). Replaces the
/// old `entry(key).or_insert(candidate)`, which always kept whichever build
/// happened to acquire the per-model lock and reach this call first — even
/// one that snapshotted a now-stale corpus — and silently discarded a
/// later, fresher build's result because `or_insert` is a no-op once the
/// key is occupied.
async fn install_if_fresher(ann: &SharedAnn, key: &AnnKey, candidate: AnnBridge) {
    let mut idxs = ann.indexes.write().await;
    match idxs.get(key) {
        Some(existing) if existing.generation >= candidate.generation => {
            tracing::debug!(
                model = %key.model,
                existing_generation = existing.generation,
                candidate_generation = candidate.generation,
                "memory ANN install skipped: cached entry is already >= this build's generation"
            );
        }
        _ => {
            idxs.insert(key.clone(), candidate);
        }
    }
}

/// Fetch (creating if absent) the per-model warm single-flight lock.
///
/// The outer `model_locks` mutex is only held long enough to look up or
/// insert the per-key `Arc<Mutex<()>>` — never across the warm attempt
/// itself, so unrelated models never contend on this map.
async fn model_warm_lock(ann: &SharedAnn, key: &AnnKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = ann.model_locks.lock().await;
    locks
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// #836 test seam: acquire and hold `key`'s per-model warm single-flight
/// lock, the same one `ensure_ann_for_model` blocks on when a concurrent
/// caller (e.g. the daemon's boot-time `warm_existing_memory_indexes`) is
/// mid-build. Returning the owned guard lets a test simulate "boot warm is
/// building this model from scratch" without depending on real build
/// latency — the guard is simply held for as long as the test wants, then
/// dropped to release it.
#[cfg(test)]
pub(crate) async fn hold_model_warm_lock_for_test(
    ann: &SharedAnn,
    key: &AnnKey,
) -> tokio::sync::OwnedMutexGuard<()> {
    model_warm_lock(ann, key).await.lock_owned().await
}

#[cfg(test)]
impl AnnState {
    pub(crate) fn warm_route_count(&self) -> usize {
        self.warm_route_count.load(Ordering::SeqCst)
    }

    pub(crate) fn reset_warm_route_count(&self) {
        self.warm_route_count.store(0, Ordering::SeqCst);
    }
}

// ── AnnBridge ─────────────────────────────────────────────────────────────────

impl AnnBridge {
    pub(crate) fn build(
        mut vectors: Vec<f32>,
        dim: usize,
        id_map: Vec<Uuid>,
        namespace_set: HashSet<String>,
    ) -> Result<Self, RuntimeError> {
        if dim == 0 {
            return Err(RuntimeError::Internal("dimension must be > 0".into()));
        }
        if vectors.is_empty() || id_map.is_empty() {
            return Err(RuntimeError::Internal(
                "no vectors to build ANN index from".into(),
            ));
        }
        let n = vectors.len() / dim;
        if n != id_map.len() {
            return Err(RuntimeError::Internal(format!(
                "id_map length {} != vector count {}",
                id_map.len(),
                n
            )));
        }
        for row in vectors.chunks_exact_mut(dim) {
            l2_normalize(row);
        }
        let cfg = VamanaConfig::with_dimensions(dim);
        let index =
            VamanaIndex::build(&vectors, cfg).map_err(|e| RuntimeError::Internal(e.to_string()))?;
        Ok(Self {
            index,
            id_map,
            namespace_set,
            generation: 0,
            epoch_baseline: 0,
        })
    }

    /// Stamp this build with the write-generation its corpus snapshot was
    /// taken at or after (#750). Set unconditionally by every install path
    /// in `ensure_ann_for_model_inner` before the compare-and-maybe-replace
    /// install; defaults to `0` (oldest possible) so any construction path
    /// that forgets to call this loses every generation comparison rather
    /// than winning one it shouldn't.
    pub(crate) fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Stamp this build with the durable corpus epoch observed at build/load
    /// time (#812). See `epoch_baseline`'s doc
    /// comment for why this is tracked separately from `generation`.
    pub(crate) fn with_epoch_baseline(mut self, epoch: u64) -> Self {
        self.epoch_baseline = epoch;
        self
    }

    pub(crate) fn search(&self, query: &[f32], k: usize) -> Result<Vec<(Uuid, f32)>, RuntimeError> {
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        let raw = self
            .index
            .search(&q, k)
            .map_err(|e| RuntimeError::Internal(format!("memory ANN search: {e}")))?;
        let hits = raw
            .into_iter()
            .filter_map(|(idx, dist)| {
                self.id_map.get(idx as usize).map(|uuid| {
                    // L2² → cosine for unit vectors: cos(a,b) = 1 - ||a-b||²/2
                    let cosine = (1.0 - dist / 2.0).max(0.0);
                    (*uuid, cosine)
                })
            })
            .collect();
        Ok(hits)
    }

    pub(crate) fn to_snapshot(
        &self,
        namespace: &str,
        model: &str,
        fingerprint: CorpusFingerprint,
    ) -> Result<VamanaSnapshot, khive_vamana::VamanaError> {
        let external_ids: Vec<String> = self.id_map.iter().map(|id| id.to_string()).collect();
        self.index
            .to_snapshot(namespace, model, fingerprint, external_ids)
    }

    pub(crate) fn from_snapshot(snapshot: VamanaSnapshot) -> Result<Self, RuntimeError> {
        let id_map: Vec<Uuid> = snapshot
            .external_ids
            .iter()
            .map(|s| {
                Uuid::parse_str(s).map_err(|e| RuntimeError::Internal(format!("bad UUID {s}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        let index = VamanaIndex::from_snapshot(&snapshot)
            .map_err(|e| RuntimeError::Internal(format!("snapshot restore: {e}")))?;
        Ok(Self {
            index,
            id_map,
            // Namespace set is populated after restore via `populate_namespace_set`.
            // Until then it is left empty, which causes the retry gate to be
            // conservative (assume the index may contain non-visible namespaces).
            namespace_set: HashSet::new(),
            generation: 0,
            epoch_baseline: 0,
        })
    }

    /// Populate `namespace_set` from an already-queried set of namespace strings.
    pub(crate) fn set_namespace_set(&mut self, ns_set: HashSet<String>) {
        self.namespace_set = ns_set;
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Replace non-alphanumeric chars with `_` to produce a valid table-name suffix.
pub(crate) fn sanitize_model_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Snapshot key for the global memory Vamana index for a model.
/// Distinct from knowledge's `{ns}::vamana::{model}` to prevent corpus identity collisions.
pub(crate) fn snapshot_key(_namespace: &str, model: &str) -> String {
    format!("global::memory_vamana::{model}")
}

const MEMORY_VAMANA_INDEX_TYPE: &str = "memory_vamana";

/// Status returned by `ensure_ann_for_model` so callers can log/act on the
/// build outcome without parsing log lines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnEnsureStatus {
    AlreadyLoaded,
    LoadedSnapshot,
    Built { vectors: usize },
    EmptyCorpus,
    DiscardedStaleBuild,
}

// ── state operations ──────────────────────────────────────────────────────────

/// Search the already-loaded index for `key`. Returns `Ok(None)` on cache miss,
/// `Ok(Some(hits))` on success, `Err` on ANN search failure (caller falls back).
pub(crate) async fn search_loaded(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Result<Option<Vec<(Uuid, f32)>>, RuntimeError> {
    let guard = ann.indexes.read().await;
    match guard.get(key) {
        None => Ok(None),
        Some(bridge) => {
            #[cfg(test)]
            ann.warm_route_count.fetch_add(1, Ordering::SeqCst);
            bridge.search(query, k).map(Some)
        }
    }
}

/// Return the namespace set for the loaded index, or `None` on cache miss.
///
/// An empty set (returned for snapshot-restored indexes before their set is
/// populated) must be treated conservatively by the caller — i.e. assume the
/// index may contain non-visible namespaces and proceed with the retry loop.
pub(crate) async fn index_namespace_set(ann: &SharedAnn, key: &AnnKey) -> Option<HashSet<String>> {
    let guard = ann.indexes.read().await;
    guard.get(key).map(|b| b.namespace_set.clone())
}

/// Remove a single in-memory ANN slot and its warming guard entry.
///
/// Only used on a genuine ANN search failure (`handlers/common.rs`) — a
/// write no longer calls anything like this (#791; see `bump_generation`'s
/// doc comment). Evicting the slot there is correct because the cached
/// entry itself just proved unusable, not merely stale.
pub(crate) async fn clear_key(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
    lock_warming(ann).remove(key);
}

/// Lock `ann.warming`, recovering from poisoning instead of propagating it.
///
/// A poisoned lock here would mean an earlier holder panicked while the
/// guard was held — but every critical section on this mutex is a plain
/// synchronous set operation, so a panic mid-section can only have come from
/// the allocator itself. Refusing to recover would leave every future warm
/// attempt permanently unable to take the guard at all, which is strictly
/// worse than the bug this fixes.
fn lock_warming(ann: &SharedAnn) -> std::sync::MutexGuard<'_, HashSet<AnnKey>> {
    ann.warming
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII release of the fire-once `warming` guard (#812).
///
/// The guard used to be removed only on the "nothing got loaded" failure
/// path inside `ensure_ann_background`'s tracked task — so a single
/// successful warm left the key in `warming` forever, and every later
/// write's rebuild attempt (`memory.remember`) plus recall's rescheduling
/// (`handlers/common.rs`) silently no-op'd against the stale guard, serving
/// the stale index indefinitely. Tying release to `Drop` instead covers
/// every exit path of the tracked task uniformly — normal return, an early
/// `Err`, and an unwinding panic all run the same destructor.
struct WarmingGuard {
    ann: SharedAnn,
    key: AnnKey,
}

impl Drop for WarmingGuard {
    fn drop(&mut self) {
        lock_warming(&self.ann).remove(&self.key);
        #[cfg(test)]
        self.ann.warming_idle.notify_waiters();
    }
}

/// Test-only: await until `key` is no longer in the `warming` single-flight
/// set (#844) — i.e. every background rebuild task that was in flight for it
/// has finished (or none ever started). Creates the `Notified` future before
/// checking the predicate so a release that lands between the check and the
/// `.await` is never missed (`tokio::sync::Notify`'s documented guarantee),
/// and loops so a chained re-enqueue that grabs the guard again after this
/// call observed a momentary release is still waited out.
#[cfg(test)]
pub(crate) async fn wait_until_warm_idle(ann: &SharedAnn, key: &AnnKey) {
    loop {
        let notified = ann.warming_idle.notified();
        if !lock_warming(ann).contains(key) {
            return;
        }
        notified.await;
    }
}

// `is_benign_shutdown_cancellation` moved to `khive_runtime::phase_events`
// (re-exported at the crate root) so ADR-103 Amendment 1's kg/knowledge
// embedder-warm phase hooks can reuse the same shutdown-cancellation
// classification this module originated, without duplicating the typed
// downcast. Imported below via `use khive_runtime::is_benign_shutdown_cancellation;`.

/// Fire-once per-model background warm. Returns `true` if a new task was started.
pub(crate) async fn ensure_ann_background(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> bool {
    if model.is_empty() {
        return false;
    }
    let key = AnnKey::from_token(token, model);

    // #750: snapshot the write-generation floor BEFORE the fast path so a
    // caller triggered by a write that just bumped the counter always sees
    // its own write reflected here — a caller reading this after a write
    // must never observe a lower floor than that write set.
    let target_generation = current_generation(ann, &key).await;

    // Fast path: already loaded AND fresh enough for this caller's write.
    // A merely-present entry is not sufficient — a concurrent build that
    // snapshotted an older corpus can still be sitting in the cache from a
    // race that finished installing after this caller's write landed (#750).
    if installed_is_fresh(ann, &key, target_generation).await {
        return false;
    }

    if !try_take_warming_guard(ann, &key) {
        return false;
    }

    // Tracked, not a bare tokio::spawn, so daemon shutdown's drain() waits for
    // an in-flight remember-path warm instead of a SIGTERM (or a short-lived
    // `kkernel exec` process exiting) aborting it mid-build — same rationale
    // as recall.rs's serve-ledger append (PR #583). The caller still only
    // pays for the enqueue; the build itself
    // runs fully off the response path, unawaited.
    spawn_rebuild_task(rt.clone(), ann.clone(), model.to_owned(), key);
    true
}

/// Synchronously take the single-flight `warming` guard for `key`. Split out
/// of `ensure_ann_background` so the post-release re-enqueue path in
/// `spawn_rebuild_task` (#812) can take the
/// guard itself and call `spawn_rebuild_task` directly instead of calling
/// back into `ensure_ann_background` — which would make the tracked task's
/// own future type recursively reference itself and fail to prove `Send`.
fn try_take_warming_guard(ann: &SharedAnn, key: &AnnKey) -> bool {
    let mut warming = lock_warming(ann);
    if warming.contains(key) {
        return false;
    }
    warming.insert(key.clone());
    true
}

/// Spawn the tracked background-rebuild loop for `key`. The caller MUST have
/// already taken the `warming` single-flight guard for `key` (via
/// `try_take_warming_guard`) before calling this — it is not re-checked
/// here.
///
/// Deliberately a plain (non-`async`) function, not folded back into
/// `ensure_ann_background`: the loop below re-enqueues itself by calling
/// this function again after releasing its guard (#812). Calling an `async
/// fn` that (transitively)
/// awaits itself makes rustc unable to prove the resulting future is `Send`
/// — the compiler reported exactly that when the re-enqueue call was
/// `ensure_ann_background(...).await` from inside this same tracked task.
/// Because `spawn_rebuild_task` is synchronous, the "recursive" call at the
/// bottom of the loop is a plain function call that hands a brand-new,
/// independent future to `track_background_task` — it never becomes part of
/// this future's own state machine, so there is no self-referential type for
/// Send-inference to choke on.
fn spawn_rebuild_task(rt: KhiveRuntime, ann: SharedAnn, model: String, key: AnnKey) {
    spawn_rebuild_task_inner(rt, ann, model, key, false);
}

/// `chained`: `true` only for the post-release re-enqueue call at the bottom
/// of this function's own tracked task — i.e. this spawn exists because a
/// PRIOR task in the same chain just exhausted its `ATTEMPT_BOUND` with more
/// work still pending. `false` for every caller-triggered spawn
/// (`ensure_ann_background`'s first call for a key). Only a chained spawn
/// pays `REBUILD_CHAIN_DEBOUNCE` before its first attempt (#812). The original
/// caller-triggered spawn must still
/// start immediately, matching every existing warm-latency expectation and
/// test in this file.
fn spawn_rebuild_task_inner(
    rt: KhiveRuntime,
    ann: SharedAnn,
    model: String,
    key: AnnKey,
    chained: bool,
) {
    // Tied to the tracked task's own scope so it releases on every exit path
    // (success, error, or panic) rather than only the "nothing got loaded"
    // arm; see `WarmingGuard`'s doc comment (#812).
    let warming_guard = WarmingGuard {
        ann: ann.clone(),
        key: key.clone(),
    };
    khive_runtime::track_background_task(async move {
        if chained {
            tokio::time::sleep(REBUILD_CHAIN_DEBOUNCE).await;
        }
        // A write landing while this task's
        // own build is in flight bumps the generation counter, but that
        // write's own `ensure_ann_background` call finds `warming` already
        // occupied by this task and silently no-ops (the fire-once guard
        // above) — nobody else is queued to pick that write up. The old
        // single-attempt body relied entirely on some LATER `memory.recall`
        // noticing `is_current` is false and re-firing this function; with
        // no further recalls the index stayed stale forever, breaking
        // ADR-107's per-write rebuild bound. Loop here instead: after each
        // attempt, re-check whether the write-generation counter moved past
        // the floor THIS attempt started from — if a write raced in during
        // the build, immediately re-enqueue another attempt against this
        // same task rather than waiting on an external caller.
        //
        // Bounded, not unconditional (#812):
        // continuous writes during every rebuild would otherwise spin this
        // task (and the `warming` guard it holds) forever. `ATTEMPT_BOUND`
        // caps consecutive rebuild attempts within a single tracked task;
        // once exhausted the remainder is left for the post-release
        // recheck below (or a later recall/write) to pick up, documented in
        // ADR-107.
        //
        // Shutdown bound (#812): this loop has no dedicated cancellation
        // signal of its own; `AnnState` carries
        // none, and this task is registered via `khive_runtime::track_background_task`,
        // whose only shutdown coordination is `daemon::drain()`'s bounded wait
        // (`KHIVE_DRAIN_TIMEOUT_SECS`, default in `daemon.rs`) followed by an
        // unconditional process exit once that deadline passes. A chain that
        // is still respawning at shutdown is therefore bounded by drain's
        // timeout, not by anything in this file; `REBUILD_CHAIN_DEBOUNCE`
        // keeps each link short enough that drain's timeout is the effective
        // ceiling on how many chain links can still be mid-flight when the
        // process is torn down.
        const ATTEMPT_BOUND: u32 = 3;
        let mut attempt_floor = current_generation(&ann, &key).await;
        let mut attempts: u32 = 0;
        loop {
            let Ok(token) = rt.authorize(Namespace::local()) else {
                break;
            };
            attempts += 1;
            #[cfg(test)]
            {
                ann.attempt_floor_notify.notify_one();
                // Two-way barrier (#812): the
                // old test only synchronized one direction — "floor
                // captured" — then let the build race ahead unconditionally,
                // so nothing actually forced the test's `bump_generation` to
                // land before this attempt's build ran. Only armed tests
                // wait here; every other test in this file never sets
                // `attempt_floor_barrier` and proceeds exactly as before.
                if attempts == 1
                    && ann
                        .attempt_floor_barrier
                        .load(std::sync::atomic::Ordering::SeqCst)
                {
                    ann.attempt_floor_release.notified().await;
                }
            }
            match ensure_ann_for_model(&rt, &token, &ann, &model).await {
                Ok(status) => {
                    tracing::debug!(?status, model = %model, "memory ANN background warm complete");
                }
                Err(e) if is_benign_shutdown_cancellation(&e) => {
                    // Expected on a short-lived process: the build's
                    // spawn_blocking was cancelled by runtime teardown, not a
                    // backend failure — don't alarm on it.
                    tracing::debug!(error = %e, model = %model, "memory ANN background warm cancelled at shutdown");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, model = %model, "memory ANN background build failed");
                    break;
                }
            }
            let now_generation = current_generation(&ann, &key).await;
            if now_generation <= attempt_floor {
                // Nothing raced in while we built — either the corpus is
                // fully caught up, or (e.g. an empty corpus with no further
                // writes) there is nothing new to catch up to. Either way,
                // looping again would spin without making progress.
                break;
            }
            if attempts >= ATTEMPT_BOUND {
                tracing::debug!(
                    model = %model,
                    attempts,
                    "memory ANN background warm hit its rebuild-attempt bound; \
                     deferring the remainder to the next recall or write"
                );
                break;
            }
            attempt_floor = now_generation;
        }
        // PR #812: release the guard BEFORE this
        // final freshness recheck, not after. The old code held the guard
        // for the rest of the async block's scope, so it only dropped once
        // this task returned — a write landing in the gap between the
        // loop's last `current_generation` read above and that implicit
        // drop would find `warming` still occupied by THIS task at
        // `ensure_ann_background`'s single-flight check, silently no-op, and
        // be stranded once the guard finally disappeared with nobody left to
        // notice. Dropping explicitly here, then re-checking, closes that
        // window: if the recheck finds the generation moved on, `warming` is
        // already free, so taking it again below starts a genuinely new task
        // instead of rejecting itself.
        drop(warming_guard);
        if current_generation(&ann, &key).await > attempt_floor
            && try_take_warming_guard(&ann, &key)
        {
            // `chained: true` — this re-enqueue is throttled by
            // `REBUILD_CHAIN_DEBOUNCE` (#812),
            // not an immediate back-to-back respawn.
            spawn_rebuild_task_inner(rt, ann, model, key, true);
        }
    });
}

/// Warm the global per-model ANN indexes at startup — skips already-loaded keys.
pub(crate) async fn warm_existing_memory_indexes(rt: &KhiveRuntime, ann: &SharedAnn) {
    let models = rt.registered_embedding_model_names();
    for model in &models {
        let token = match rt.authorize(Namespace::local()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match ensure_ann_for_model(rt, &token, ann, model).await {
            Ok(status) => {
                tracing::debug!(?status, model = %model, "memory ANN warm complete");
            }
            Err(e) => {
                tracing::warn!(error = %e, model = %model, "memory ANN warm failed");
            }
        }
    }
}

/// Lazy warm-load for the global index for `model`: snapshot restore or rebuild with double-fingerprint check.
///
/// ADR-103 Stage 1 / issue #723 ask 1: brackets the whole attempt (snapshot
/// load and/or rebuild-from-scratch) as one `ann_warm` phase span, so an
/// operator can attribute a cold-start or on-demand-warm CPU window after
/// the fact from the `PhaseStarted`/`PhaseCompleted`/`PhaseCancelled` event
/// trio. This is the chokepoint every warm path converges on —
/// `warm_existing_memory_indexes` (daemon-startup cold warm),
/// `ensure_ann_background` (fire-once recall/remember-triggered background
/// warm), and the recall-miss path in `handlers/common.rs` (synchronous
/// on-demand warm) all call this function directly or indirectly.
///
/// Issue #723: because three independent call
/// sites can race for the same model (e.g. boot warm still running when a
/// recall misses), single-flight ownership lives *here*, not in any one
/// caller — `ensure_ann_background`'s own `warming` guard only dedups its
/// own fire-once spawns, it says nothing about a concurrent direct caller.
/// A second caller blocks on the per-model lock below and, once it acquires
/// it, re-checks `indexes` — if the first caller already warmed the model,
/// the second returns `AlreadyLoaded` immediately without repeating the
/// snapshot/rebuild attempt or emitting a second phase-span pair.
///
/// Emission is best-effort and a no-op when this `KhiveRuntime` has no
/// `EventStore` configured, matching ADR-094's existing lifecycle-event
/// emission contract exactly.
pub(crate) async fn ensure_ann_for_model(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> Result<AnnEnsureStatus, RuntimeError> {
    if model.is_empty() {
        return Ok(AnnEnsureStatus::EmptyCorpus);
    }
    let key = AnnKey::from_token(token, model);

    // #750: capture the write-generation floor BEFORE anything else — before
    // even the fast "already loaded" check — so a caller invoked right after
    // a write (memory.remember bumps the counter, then calls this) always
    // requires at least that write's generation from whatever it accepts as
    // "fresh enough". Read-generation-first, snapshot-corpus-second is the
    // ordering that closes the race: reversing it would let a write land
    // between the corpus snapshot and the generation read, understating the
    // floor and letting a build miss that write without detecting it.
    let target_generation = current_generation(ann, &key).await;

    // Fast path: no lock needed if already warm AND fresh enough.
    if installed_is_fresh(ann, &key, target_generation).await {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    let lock = model_warm_lock(ann, &key).await;
    let _single_flight_guard = lock.lock().await;

    // Re-check after acquiring the lock: a concurrent caller may have
    // finished warming this model (with a generation >= ours) while we were
    // waiting for the guard.
    if installed_is_fresh(ann, &key, target_generation).await {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    let phase_start = std::time::Instant::now();
    // Issue #723: `process_resource_usage()`
    // returns cumulative process CPU since start, so a single post-warm read
    // is unusable for per-phase attribution on a long-lived daemon (a warm
    // that runs after the process has already burned minutes of CPU would
    // report that whole cumulative total as the phase's cost). Snapshot at
    // entry too and report end-minus-start below.
    let cpu_start = khive_runtime::process_resource_usage();
    // Held for the lifetime of this call so `comm.health`'s resource
    // self-report (#723 ask 2) can see `ann_warm` in its active-phases list
    // while this warm/rebuild is in flight. Dropped (and the gauge
    // decremented) on every exit path, including an early `?`-propagated
    // error, since it is a plain RAII guard.
    let _phase_guard = khive_runtime::register_active_phase("ann_warm");
    // Best-effort, cheap COUNT(*) — `None` if the query itself fails, which
    // is fine: `corpus_size` on the Started row is a diagnostic nicety, not
    // load-bearing for anything downstream.
    let corpus_size = compute_memory_fingerprint(rt, token, model)
        .await
        .map(|fp| fp.vector_count);
    emit_ann_warm_phase_event(
        rt,
        token,
        model,
        khive_types::EventKind::PhaseStarted,
        khive_storage::PhaseStartedPayload {
            work_class: "warm".into(),
            phase: "ann_warm".into(),
            corpus_size,
        },
    )
    .await;

    let result = ensure_ann_for_model_inner(rt, token, ann, model, target_generation).await;

    let wall_us = phase_start.elapsed().as_micros() as i64;
    let cpu_us = khive_runtime::cpu_delta_us(cpu_start, khive_runtime::process_resource_usage());
    match &result {
        Err(e) if is_benign_shutdown_cancellation(e) => {
            emit_ann_warm_phase_event(
                rt,
                token,
                model,
                khive_types::EventKind::PhaseCancelled,
                khive_storage::PhaseCancelledPayload {
                    work_class: "warm".into(),
                    phase: "ann_warm".into(),
                    wall_us,
                    cpu_us,
                },
            )
            .await;
        }
        _ => {
            emit_ann_warm_phase_event(
                rt,
                token,
                model,
                khive_types::EventKind::PhaseCompleted,
                khive_storage::PhaseCompletedPayload {
                    work_class: "warm".into(),
                    phase: "ann_warm".into(),
                    wall_us,
                    cpu_us,
                },
            )
            .await;
        }
    }
    result
}

/// Append one ADR-103 Stage 1 `ann_warm` phase-span event, logging and
/// swallowing store/serialize failures — the phase-log path must never
/// interrupt or slow down the warm/rebuild it is observing (same rule as
/// `khive-db::checkpoint`'s lifecycle-event helper).
async fn emit_ann_warm_phase_event<P: serde::Serialize>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    kind: khive_types::EventKind,
    payload: P,
) {
    // Best-effort exactly like ADR-094's other lifecycle-event emitters: a
    // backend that cannot resolve an `EventStore` for this token's namespace
    // is treated as an unconfigured audit sink, not an error to propagate.
    let Ok(store) = rt.events(token) else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_kind = %kind.name(),
                model,
                "failed to serialize ann_warm phase event payload"
            );
            return;
        }
    };
    let event = khive_storage::Event::new(
        token.namespace().as_str(),
        "memory.ann_warm",
        kind,
        khive_types::SubstrateKind::Event,
        "daemon:ann_warm",
    )
    .with_payload(payload_value);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            event_kind = %kind.name(),
            model,
            "ann_warm phase event append failed"
        );
    }
}

/// Original `ensure_ann_for_model` body: snapshot restore or rebuild with
/// double-fingerprint check, split out so [`ensure_ann_for_model`] can
/// bracket it with ADR-103 Stage 1 phase-span emission above.
async fn ensure_ann_for_model_inner(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    target_generation: u64,
) -> Result<AnnEnsureStatus, RuntimeError> {
    let ns = "global";
    let key = AnnKey::new(ns, model);

    if installed_is_fresh(ann, &key, target_generation).await {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    // Captured once per attempt, same rationale as `target_generation`
    // above: whatever gets installed by this attempt (snapshot-load or
    // rebuild) is stamped with the durable epoch observed here, so a LATER
    // `maybe_check_durable_epoch` call only flags this entry stale once a
    // reindex bumps the epoch again after this point (#812).
    let target_epoch = durable_epoch(rt).await;

    // Try snapshot warm-load. Both the shallow `CorpusFingerprint` (vector
    // count + dimensions) AND the durable `CorpusContentHash` (a blake3 hash
    // of the exact ordered subject-id + embedding-byte rows a build consumes)
    // must match the live corpus (#812):
    // write-generations reset to 0 on restart, so on a cold process the
    // fingerprint alone is the only signal deciding Hot vs. Stale — and a
    // delete-one/add-one, a content re-embed, OR a vector-only re-embed
    // (`kkernel reindex`, which overwrites embeddings without touching
    // `notes.updated_at`) all preserve vector count and dimensions exactly
    // while changing what the corpus actually contains. Hashing the raw
    // embedding bytes themselves (not a proxy like `updated_at`) catches all
    // three, and computing it here from a fresh scan of the SAME rows a
    // rebuild would read — rather than a separate cheap aggregate query
    // sampled at a different instant — closes the race where a
    // same-cardinality write lands between "what the graph was built from"
    // and "what the signal was sampled from".
    if let Some(persisted) = try_load_snapshot(rt, ns, model).await {
        let current_fp = compute_memory_fingerprint(rt, token, model).await;
        let fp_matches = current_fp.is_some_and(|fp| persisted.snapshot.fingerprint == fp);
        // Only pay for the full corpus-hash scan when the cheap fingerprint
        // already agrees — a fingerprint mismatch alone is enough to know a
        // rebuild is needed.
        let hash_matches = fp_matches
            && compute_corpus_content_hash(rt, token, model)
                .await
                .is_some_and(|hash| persisted.content_hash == hash);
        if fp_matches && hash_matches {
            match AnnBridge::from_snapshot(persisted.snapshot) {
                Ok(mut bridge) => {
                    // Populate namespace set from a cheap DISTINCT query so the
                    // retry gate in recall can short-circuit when appropriate.
                    let ns_set = query_distinct_namespaces(rt, token, model)
                        .await
                        .unwrap_or_default();
                    bridge.set_namespace_set(ns_set);
                    let bridge = bridge
                        .with_generation(target_generation)
                        .with_epoch_baseline(target_epoch);
                    install_if_fresher(ann, &key, bridge).await;
                    tracing::debug!(namespace = %ns, model = %model, "memory ANN loaded from snapshot");
                    return Ok(AnnEnsureStatus::LoadedSnapshot);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "corrupt memory Vamana snapshot; rebuilding");
                }
            }
        } else {
            tracing::info!(
                namespace = %ns,
                model = %model,
                fp_matches,
                hash_matches,
                "stale memory Vamana snapshot (fingerprint or content-hash mismatch); rebuilding"
            );
        }
    }

    // Rebuild from vector store with double-fingerprint concurrency check.
    // The fingerprint sandwich alone (compute before, scan, compute after)
    // only bounds the SCAN window — it cannot see a write that lands after
    // `fp_after` is read but before this build's `install_if_fresher` call
    // below (e.g. during `persist_snapshot`'s I/O). The write-generation
    // check closes that residual window: `target_generation` was captured
    // in the caller BEFORE this whole attempt started, so a write landing
    // after that point bumps the counter to a strictly higher value that a
    // later, correctly-scoped rebuild will carry on its own bridge —
    // `install_if_fresher` then refuses to let THIS (now-stale) build
    // overwrite that later result, regardless of which one finishes first.
    let fp_before = compute_memory_fingerprint(rt, token, model).await;
    match load_and_build_from_vector_store(rt, token, model).await {
        Ok(Some((bridge, content_hash))) => {
            let fp_after = compute_memory_fingerprint(rt, token, model).await;
            // If fingerprint changed during the scan, the corpus raced; discard.
            if fp_before != fp_after {
                tracing::debug!(
                    namespace = %ns,
                    model = %model,
                    "memory ANN corpus mutated during build; discarding"
                );
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            let vector_count = bridge.id_map.len();
            let bridge = bridge
                .with_generation(target_generation)
                .with_epoch_baseline(target_epoch);
            if let Some(fingerprint) = fp_after {
                // `content_hash` was computed from the exact rows this
                // build's own scan read (#812),
                // no separate sampling read, so there is no window between
                // "what the graph was built from" and "what got persisted
                // as the freshness signal" for a race to land in.
                if let Err(e) =
                    persist_snapshot(rt, ns, model, &bridge, fingerprint, content_hash).await
                {
                    tracing::warn!(error = %e, "failed to persist memory Vamana snapshot");
                }
            }
            install_if_fresher(ann, &key, bridge).await;
            tracing::debug!(namespace = %ns, model = %model, vectors = vector_count, "memory ANN index built");
            Ok(AnnEnsureStatus::Built {
                vectors: vector_count,
            })
        }
        Ok(None) => {
            tracing::debug!(namespace = %ns, model = %model, "memory ANN: no note vectors to build");
            Ok(AnnEnsureStatus::EmptyCorpus)
        }
        Err(e) if is_benign_shutdown_cancellation(&e) => {
            // Same benign-cancellation case as ensure_ann_background's own
            // classification below: this arm fires first (before the error
            // propagates to the background-warm caller), so it must be
            // downgraded here too or a cancelled background warm still logs
            // one WARN line from this site alone.
            tracing::debug!(error = %e, namespace = %ns, model = %model, "memory ANN build cancelled at shutdown");
            Err(e)
        }
        Err(e) => {
            tracing::warn!(error = %e, namespace = %ns, model = %model, "memory ANN build failed");
            Err(e)
        }
    }
}

// ── corpus loading ────────────────────────────────────────────────────────────

/// Query the set of distinct `namespace` values present in the vector corpus for `model`.
/// Used after snapshot restore to populate the in-memory namespace set.
async fn query_distinct_namespaces(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<HashSet<String>> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let _ = store; // ensure store is accessible; actual query goes to SQL layer
    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT DISTINCT n.namespace FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_distinct_namespaces".into()),
        })
        .await
        .ok()?;
    let set: HashSet<String> = rows
        .into_iter()
        .filter_map(|row| {
            if let Some(SqlValue::Text(ns)) = row.get("namespace") {
                Some(ns.clone())
            } else {
                None
            }
        })
        .collect();
    Some(set)
}

/// Compute a fingerprint for all non-deleted memory note vectors for `model` (all namespaces).
async fn compute_memory_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let info = store.info().await.ok()?;
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT COUNT(*) AS n FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_fingerprint".into()),
        })
        .await
        .ok()?;
    let vector_count = match rows.first()?.get("n")? {
        SqlValue::Integer(n) if *n >= 0 => *n as u64,
        _ => return None,
    };
    Some(CorpusFingerprint {
        vector_count,
        dimensions: info.dimensions as u32,
    })
}

/// Scan all non-deleted `note.content` vectors across all namespaces for `model`, build an
/// `AnnBridge`, and hash the exact rows the build consumed (#812). Returns
/// `Ok(None)` when the corpus is empty or inaccessible.
async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<(AnnBridge, CorpusContentHash)>, RuntimeError> {
    let store = match rt.vectors_for_model(token, model) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    // Plain `?` (not `.map_err(RuntimeError::Internal(e.to_string()))`) so the
    // typed `StorageError` — and, when a background warm's spawn_blocking is
    // cancelled at shutdown, the `tokio::task::JoinError` boxed inside it —
    // survives to `is_benign_shutdown_cancellation`'s downcast instead of
    // being collapsed into an opaque string.
    let info = store.info().await?;
    if info.dimensions == 0 {
        return Ok(None);
    }
    let dims = info.dimensions;

    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");

    let sql = rt.sql();
    let mut reader = sql.reader().await?;

    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT v.subject_id, v.embedding, n.namespace FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL \
                 ORDER BY v.subject_id"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_corpus_scan".into()),
        })
        .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut id_map: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut flat: Vec<f32> = Vec::with_capacity(rows.len() * dims);
    let mut namespace_set: HashSet<String> = HashSet::new();
    // Hashed row-by-row, in the SAME loop that feeds `flat`/`id_map` below —
    // not a separate query run before or after this scan — so the persisted
    // signal can never describe a different corpus snapshot than the graph
    // that gets built from it (#812). Hashes the
    // raw, un-normalized embedding bytes (not `notes.updated_at`), so a
    // vector-only re-embed that never touches the notes table — exactly
    // what `kkernel reindex` does, still changes the hash (#812).
    let mut hasher = blake3::Hasher::new();

    for row in &rows {
        let id_str = match row.get("subject_id") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let uuid = match Uuid::parse_str(id_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(b)) => b.as_slice(),
            _ => continue,
        };
        if bytes.len() != dims * 4 {
            continue;
        }
        let vec: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if let Some(SqlValue::Text(ns)) = row.get("namespace") {
            namespace_set.insert(ns.clone());
        }
        hasher.update(uuid.as_bytes());
        hasher.update(bytes);
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }

    if id_map.is_empty() {
        return Ok(None);
    }

    let content_hash = CorpusContentHash(*hasher.finalize().as_bytes());

    // #791: `AnnBridge::build` (via `VamanaIndex::build`) is a plain
    // synchronous, CPU-bound full graph construction — training the SQ8
    // quantization codec and building the Vamana graph over the whole
    // corpus scanned above. Run it on the blocking thread pool so it never
    // monopolizes the tokio worker driving this future (and every other
    // task scheduled on it) for the full build duration, whether this call
    // is reached from a background warm or, on a genuine cold-start miss,
    // directly on a recall's own request path.
    let built =
        tokio::task::spawn_blocking(move || AnnBridge::build(flat, dims, id_map, namespace_set))
            .await
            .map_err(|e| {
                RuntimeError::Storage(StorageError::driver(
                    khive_storage::StorageCapability::Vectors,
                    "memory_ann_build",
                    e,
                ))
            })??;
    Ok(Some((built, content_hash)))
}

// ── persistence ───────────────────────────────────────────────────────────────

/// Durable content signal for a model's live corpus (#812).
///
/// `CorpusFingerprint` (vector count + dimensions) is preserved by a
/// delete-one/add-one or a content re-embed, and write-generations
/// (`AnnState::generations`) are in-memory-only and reset to 0 on restart —
/// so on a cold process the fingerprint is the only signal deciding whether
/// a persisted snapshot is still current, and a same-cardinality corpus
/// change would be classified Hot forever with no rebuild ever triggered.
/// A blake3 hash of the ordered `(subject_id, embedding)` rows closes that
/// gap completely: any change to which rows exist, their ordering, or their
/// embedding bytes changes the hash — including a vector-only re-embed
/// (`kkernel reindex`) that never touches `notes.updated_at` at all, which a
/// timestamp-based signal cannot see (#812).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CorpusContentHash([u8; 32]);

/// On-disk wrapper persisted into `retrieval_snapshots.snapshot` in place of
/// a bare `VamanaSnapshot`, so the durable content hash travels with the
/// snapshot it was computed against. A pre-existing bare-`VamanaSnapshot`
/// blob (persisted before this field existed) fails to deserialize as this
/// wrapper and is treated as corrupt by `try_load_snapshot`'s existing
/// corrupt-blob handling — self-healing, since the next warm attempt
/// rebuilds and re-persists in the new format.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedMemorySnapshot {
    snapshot: VamanaSnapshot,
    content_hash: CorpusContentHash,
}

/// Recompute `model`'s durable content hash directly from the store (#812
/// review re-confirm HIGH-A/HIGH-B), for restart validation ONLY — the build
/// path computes its own hash from the exact rows it scans in
/// `load_and_build_from_vector_store`, in the same read, rather than calling
/// this. Deliberately NOT a cheap aggregate: it re-reads every live
/// `note.content` embedding blob for `model`, in the same row order
/// (`ORDER BY v.subject_id`) the build itself uses, so a match here means
/// "the persisted snapshot was built from exactly this corpus" rather than
/// merely "the row count and a timestamp line up".
async fn compute_corpus_content_hash(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusContentHash> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let info = store.info().await.ok()?;
    if info.dimensions == 0 {
        return None;
    }
    let dims = info.dimensions;
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT v.subject_id, v.embedding FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL \
                 ORDER BY v.subject_id"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_content_hash".into()),
        })
        .await
        .ok()?;

    let mut hasher = blake3::Hasher::new();
    let mut any_row = false;
    for row in &rows {
        let id_str = match row.get("subject_id") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let uuid = match Uuid::parse_str(id_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(b)) => b.as_slice(),
            _ => continue,
        };
        if bytes.len() != dims * 4 {
            continue;
        }
        hasher.update(uuid.as_bytes());
        hasher.update(bytes);
        any_row = true;
    }
    if !any_row {
        return None;
    }
    Some(CorpusContentHash(*hasher.finalize().as_bytes()))
}

async fn ensure_snapshot_schema(rt: &KhiveRuntime) -> Result<(), RuntimeError> {
    let sql = rt.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    w.execute_script(
        r#"
        CREATE TABLE IF NOT EXISTS retrieval_snapshots (
            namespace   TEXT NOT NULL,
            index_type  TEXT NOT NULL,
            snapshot    BLOB NOT NULL,
            created_at  INTEGER NOT NULL,
            PRIMARY KEY (namespace, index_type)
        );
        CREATE INDEX IF NOT EXISTS idx_retrieval_snapshots_namespace
            ON retrieval_snapshots(namespace);
        "#
        .into(),
    )
    .await
    .map_err(|e| RuntimeError::Internal(e.to_string()))
}

async fn persist_snapshot(
    rt: &KhiveRuntime,
    namespace: &str,
    model: &str,
    bridge: &AnnBridge,
    fingerprint: CorpusFingerprint,
    content_hash: CorpusContentHash,
) -> Result<(), RuntimeError> {
    if let Err(e) = ensure_snapshot_schema(rt).await {
        tracing::warn!(error = %e, "failed to create retrieval_snapshots schema");
        return Err(e);
    }
    let snapshot = bridge
        .to_snapshot(namespace, model, fingerprint)
        .map_err(|e| RuntimeError::Internal(format!("to_snapshot: {e}")))?;
    let persisted = PersistedMemorySnapshot {
        snapshot,
        content_hash,
    };
    let blob = serde_json::to_vec(&persisted)
        .map_err(|e| RuntimeError::Internal(format!("snapshot serialize: {e}")))?;
    let key = snapshot_key(namespace, model);
    let sql = rt.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    w.execute(SqlStatement {
        sql: "INSERT OR REPLACE INTO retrieval_snapshots \
              (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, ?4)"
            .into(),
        params: vec![
            SqlValue::Text(key),
            SqlValue::Text(MEMORY_VAMANA_INDEX_TYPE.into()),
            SqlValue::Blob(blob),
            SqlValue::Integer(0),
        ],
        label: Some("persist_memory_vamana_snapshot".into()),
    })
    .await
    .map(|_| ())
    .map_err(|e| RuntimeError::Internal(e.to_string()))
}

async fn try_load_snapshot(
    rt: &KhiveRuntime,
    namespace: &str,
    model: &str,
) -> Option<PersistedMemorySnapshot> {
    let key = snapshot_key(namespace, model);
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT snapshot FROM retrieval_snapshots \
                  WHERE namespace = ?1 AND index_type = ?2"
                .into(),
            params: vec![
                SqlValue::Text(key),
                SqlValue::Text(MEMORY_VAMANA_INDEX_TYPE.into()),
            ],
            label: None,
        })
        .await
        .ok()?;
    let row = rows.into_iter().next()?;
    let blob = match row.get("snapshot")? {
        SqlValue::Blob(b) => b.clone(),
        _ => return None,
    };
    match serde_json::from_slice::<PersistedMemorySnapshot>(&blob) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "corrupt memory Vamana snapshot blob");
            None
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn ann_key_is_model_only() {
        // After FTS+ANN consolidation AnnKey is model-only; namespace is ignored.
        let k1 = AnnKey::new("ns:a", "model-x");
        let k2 = AnnKey::new("ns:b", "model-x"); // same model, different ns → same key
        let k3 = AnnKey::new("ns:a", "model-y"); // different model → different key
        assert_eq!(
            k1, k2,
            "same model, different namespace must produce the same key"
        );
        assert_ne!(k1, k3, "different models must produce different keys");
    }

    #[test]
    fn ann_bridge_maps_vamana_ids_to_uuids() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        // 3 orthogonal unit vectors in 3D
        let vectors = vec![
            1.0f32, 0.0, 0.0, // id_a
            0.0, 1.0, 0.0, // id_b
            0.0, 0.0, 1.0, // id_c
        ];
        let bridge =
            AnnBridge::build(vectors, 3, vec![id_a, id_b, id_c], HashSet::new()).expect("build");

        // query close to id_a
        let hits = bridge.search(&[1.0, 0.0, 0.0], 1).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, id_a, "nearest to [1,0,0] must be id_a");
        assert!(hits[0].1 > 0.9, "cosine must be close to 1.0");
    }

    #[test]
    fn ann_search_dimension_error_returns_err() {
        let id = Uuid::new_v4();
        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0], 3, vec![id], HashSet::new())
            .expect("build 3-dim bridge");
        // query with wrong dimension (2 instead of 3)
        let result = bridge.search(&[1.0, 0.0], 1);
        assert!(result.is_err(), "wrong dimension must return Err");
    }

    #[test]
    fn snapshot_key_does_not_collide_with_knowledge_vamana() {
        let mem_key = snapshot_key("local", "all-minilm-l6-v2");
        assert!(
            mem_key.contains("::memory_vamana::"),
            "memory key must contain ::memory_vamana:: but got: {mem_key}"
        );
        assert!(
            !mem_key.contains("::vamana::"),
            "memory key must not match knowledge pattern ::vamana:: but got: {mem_key}"
        );
    }

    // ── #791: writes no longer destroy the fast fallback ────────────────────
    //
    // `memory.remember`/`memory.prune`/the KG note-mutation hook used to call
    // `ann::invalidate_namespace`, which cleared every in-memory index slot
    // AND deleted the persisted snapshot row synchronously on the write's own
    // path. A `memory.recall` landing after that clear and before the
    // (correctly backgrounded) rebuild finished had nothing left to serve
    // except a full synchronous corpus rebuild inline on its own request
    // path — issue #791's hang. The fix: a write now only bumps the model's
    // write-generation counter (`bump_generation`, #750 machinery); the
    // previous index and snapshot stay installed and keep serving reads
    // until a fresher build replaces them via `install_if_fresher`.

    #[tokio::test]
    async fn bump_generation_does_not_evict_installed_index_or_snapshot() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let key = AnnKey::new("global", "model-x");
        let id = Uuid::new_v4();

        install_if_fresher(&ann, &key, tiny_bridge(id, 1)).await;
        {
            let idxs = ann.indexes.read().await;
            let bridge = idxs.get(&key).expect("installed above");
            persist_snapshot(
                &rt,
                "global",
                "model-x",
                bridge,
                CorpusFingerprint {
                    vector_count: 1,
                    dimensions: 4,
                },
                CorpusContentHash([0u8; 32]),
            )
            .await
            .expect("persist snapshot");
        }

        // A write lands: it bumps the generation but must not clear anything.
        bump_generation(&ann, &key).await;

        assert!(
            ann.indexes.read().await.contains_key(&key),
            "a write must not evict the previously-installed in-memory index"
        );
        assert!(
            try_load_snapshot(&rt, "global", "model-x").await.is_some(),
            "a write must not delete the previously-persisted snapshot before \
             a fresher build has durably replaced it"
        );
    }

    /// The primitive both `handlers/common.rs`'s recall-path stale-serve and
    /// the write paths' no-longer-clearing behavior depend on: `search_loaded`
    /// answers from whatever is installed regardless of `is_current`, so a
    /// cache entry that has fallen behind the write-generation counter is
    /// still served — not treated as a miss that forces a synchronous
    /// rebuild on the caller's own request path. Fail-on-revert: restoring
    /// the old `if cache_fresh { search_loaded(...) } else { Ok(None) }` gate
    /// in `common.rs` would make an equivalent check on this same primitive
    /// return `None` here.
    #[tokio::test]
    async fn search_loaded_serves_stale_installed_entry_without_rebuild() {
        let ann = new_shared();
        let key = AnnKey::new("global", "model-x");
        let id = Uuid::new_v4();

        install_if_fresher(&ann, &key, tiny_bridge(id, 1)).await;
        bump_generation(&ann, &key).await; // counter -> 1
        bump_generation(&ann, &key).await; // counter -> 2, ahead of installed gen 1

        assert!(
            !is_current(&ann, &key).await,
            "sanity: the installed entry must now be behind the write-generation counter"
        );

        let hits = search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .expect("search_loaded must not error on a stale-but-installed entry");
        assert!(
            hits.is_some(),
            "a stale-but-installed entry must still be served by search_loaded, \
             not treated the same as a genuine cache miss"
        );
    }

    // ── #750: write-generation-checked install ──────────────────────────────
    //
    // The end-to-end race (a slow build snapshotting a stale corpus,
    // finishing after a newer write's invalidate+re-warm cycle, and
    // clobbering the cache) requires precise control over async task
    // interleaving that no test-only pause hook currently exists for in
    // `ensure_ann_for_model_inner` — adding one solely to force a specific
    // schedule would test the hook, not the production code path. These
    // tests instead pin down the exact invariant the fix depends on
    // directly: `install_if_fresher`'s compare-and-replace semantics, and
    // `is_current`'s stale-is-a-miss semantics. Both are unconditional,
    // deterministic properties of the fix — no timing required to observe
    // them — and the ns733/ns733b recall tests in `handlers/recall.rs`
    // additionally exercise the real end-to-end `memory.remember` →
    // `memory.recall` path stress-verified over 50 consecutive fresh-process
    // runs each (see the #750 implementation report).

    fn tiny_bridge(id: Uuid, generation: u64) -> AnnBridge {
        AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![id], HashSet::new())
            .expect("build tiny bridge")
            .with_generation(generation)
    }

    /// A candidate with a STRICTLY OLDER generation than the currently
    /// installed entry must never replace it. This is the exact shape of
    /// the pre-#750 bug: a slow build (older generation) finishing after a
    /// faster, newer-generation build already installed.
    #[tokio::test]
    async fn install_if_fresher_rejects_older_generation_candidate() {
        let ann = new_shared();
        let key = AnnKey::new("any-ns", "model-x");
        let newer_id = Uuid::new_v4();
        let older_id = Uuid::new_v4();

        install_if_fresher(&ann, &key, tiny_bridge(newer_id, 5)).await;
        install_if_fresher(&ann, &key, tiny_bridge(older_id, 2)).await;

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("an entry must be installed");
        assert_eq!(bridge.generation, 5, "the newer generation must survive");
        assert_eq!(
            bridge.id_map,
            vec![newer_id],
            "the older-generation candidate must not have replaced it"
        );
    }

    /// A candidate with a STRICTLY NEWER generation must replace an
    /// existing older entry — the compare-and-replace half of the fix
    /// (`entry(key).or_insert(...)` never replaced anything once a key was
    /// occupied, which is the direct cause of "later, more-complete
    /// rebuild attempts are discarded").
    #[tokio::test]
    async fn install_if_fresher_replaces_older_installed_entry() {
        let ann = new_shared();
        let key = AnnKey::new("any-ns", "model-x");
        let older_id = Uuid::new_v4();
        let newer_id = Uuid::new_v4();

        install_if_fresher(&ann, &key, tiny_bridge(older_id, 1)).await;
        install_if_fresher(&ann, &key, tiny_bridge(newer_id, 9)).await;

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("an entry must be installed");
        assert_eq!(bridge.generation, 9);
        assert_eq!(bridge.id_map, vec![newer_id]);
    }

    /// Equal generations: the existing entry is kept (no-op), matching the
    /// `>=` comparison in `install_if_fresher` — ties do not thrash the
    /// cache with an equivalent rebuild.
    #[tokio::test]
    async fn install_if_fresher_keeps_existing_entry_on_equal_generation() {
        let ann = new_shared();
        let key = AnnKey::new("any-ns", "model-x");
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();

        install_if_fresher(&ann, &key, tiny_bridge(first_id, 3)).await;
        install_if_fresher(&ann, &key, tiny_bridge(second_id, 3)).await;

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("an entry must be installed");
        assert_eq!(
            bridge.id_map,
            vec![first_id],
            "on an equal generation, the first-installed entry must be kept"
        );
    }

    /// `is_current` (the recall-path freshness gate) must treat a cached
    /// entry whose generation is behind the model's current write-generation
    /// counter as NOT current — the other half of the fix, since a
    /// presence-only check would still serve a stale-but-installed entry to
    /// a recall issued after a later write.
    #[tokio::test]
    async fn is_current_false_when_installed_generation_behind_counter() {
        let ann = new_shared();
        let key = AnnKey::new("any-ns", "model-x");

        // Install a bridge stamped with generation 1 (as if built before any
        // write bumped the counter further).
        install_if_fresher(&ann, &key, tiny_bridge(Uuid::new_v4(), 1)).await;
        assert!(
            is_current(&ann, &key).await,
            "with no bumps yet, generation-1 must be considered current (counter starts at 0)"
        );

        // A write lands and bumps the counter past the installed generation.
        bump_generation(&ann, &key).await; // -> 1
        bump_generation(&ann, &key).await; // -> 2
        assert!(
            !is_current(&ann, &key).await,
            "installed generation (1) is now behind the write-generation counter (2)"
        );

        // Once a fresher build (generation >= 2) installs, it is current again.
        install_if_fresher(&ann, &key, tiny_bridge(Uuid::new_v4(), 2)).await;
        assert!(
            is_current(&ann, &key).await,
            "installed generation (2) now matches the write-generation counter (2)"
        );
    }

    /// `is_current` on an absent key is false (a genuine cache miss), so
    /// callers correctly fall through to the ensure/build path rather than
    /// treating "no entry" as "no problem."
    #[tokio::test]
    async fn is_current_false_when_absent() {
        let ann = new_shared();
        let key = AnnKey::new("any-ns", "model-x");
        assert!(!is_current(&ann, &key).await);
    }

    // ADR-103 Stage 1 / issue #723 ask 1: `ensure_ann_for_model` must bracket
    // its whole attempt with a `PhaseStarted`/`PhaseCompleted` event pair
    // whenever an `EventStore` is configured, regardless of which path
    // inside it runs (here: `EmptyCorpus`, since no vectors are registered
    // for `model`). `khive_runtime::register_active_phase`'s own guard
    // release behavior (used here to populate `comm.health`'s
    // `active_phases`) is covered in isolation by
    // `khive-runtime`'s own `daemon::tests` — not re-asserted here via the
    // process-wide gauge, since that gauge is shared across this whole test
    // binary (any concurrently running `memory.remember`/`memory.recall`
    // test can trigger its own background `ensure_ann_background` warm) and
    // asserting its global emptiness here would be inherently racy.
    #[tokio::test]
    async fn ensure_ann_for_model_emits_phase_started_and_completed_events() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let ann = new_shared();
        let model = "ann-warm-phase-event-test-model";

        let status = ensure_ann_for_model(&rt, &token, &ann, model)
            .await
            .expect("ensure_ann_for_model must succeed on an empty corpus");
        assert!(matches!(status, AnnEnsureStatus::EmptyCorpus));

        let store = rt.events(&token).expect("event store for local namespace");
        let page = store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::types::PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query_events");

        let started = page
            .items
            .iter()
            .filter(|e| e.kind == khive_types::EventKind::PhaseStarted)
            .count();
        let completed = page
            .items
            .iter()
            .filter(|e| e.kind == khive_types::EventKind::PhaseCompleted)
            .count();
        let cancelled = page
            .items
            .iter()
            .filter(|e| e.kind == khive_types::EventKind::PhaseCancelled)
            .count();
        assert_eq!(started, 1, "exactly one PhaseStarted row, got: {page:?}");
        assert_eq!(
            completed, 1,
            "exactly one PhaseCompleted row, got: {page:?}"
        );
        assert_eq!(cancelled, 0, "no PhaseCancelled row on a normal completion");
    }

    // Issue #723: two concurrent callers warming
    // the same model (mirroring boot warm racing a recall-miss warm) must
    // not both run the snapshot/rebuild attempt and emit their own
    // PhaseStarted/PhaseCompleted pair. Seeds real vector rows (via a
    // deterministic hash-based embedder, same pattern as
    // `pack.rs::ann_route_tests`) so the first caller's build actually
    // populates `ann.indexes` — a prerequisite for the second caller's
    // post-lock re-check to observe it and short-circuit.
    //
    // Fail-on-revert proof: reverting the per-model single-flight lock in
    // `ensure_ann_for_model` back to "just call `ensure_ann_for_model_inner`
    // unconditionally" makes both concurrent calls run the full
    // snapshot/rebuild attempt and each emit their own PhaseStarted row,
    // failing the `started == 1` assertion below.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ensure_ann_for_model_concurrent_callers_emit_one_phase_pair() {
        use async_trait::async_trait;
        use khive_runtime::{EmbedderProvider, RuntimeConfig};
        use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

        struct HashVecService {
            dims: usize,
        }

        fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in text.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0001_0000_01b3);
            }
            let mut v = Vec::with_capacity(dims);
            let mut s = h;
            for _ in 0..dims {
                s = s
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
            }
            v
        }

        #[async_trait]
        impl EmbeddingService for HashVecService {
            async fn embed(
                &self,
                texts: &[String],
                _model: EmbeddingModel,
            ) -> Result<Vec<Vec<f32>>, EmbedError> {
                Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
            }

            fn supports_model(&self, _model: EmbeddingModel) -> bool {
                true
            }

            fn name(&self) -> &'static str {
                "hash-vec"
            }
        }

        struct HashVecProvider {
            model_name: String,
            dims: usize,
        }

        #[async_trait]
        impl EmbedderProvider for HashVecProvider {
            fn name(&self) -> &str {
                &self.model_name
            }

            fn dimensions(&self) -> usize {
                self.dims
            }

            async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
                Ok(Arc::new(HashVecService { dims: self.dims }))
            }
        }

        let tmp = tempfile::Builder::new()
            .prefix("khive-memory-ann-single-flight-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp db dir");
        let db_path = tmp.path().join("khive-graph.db");

        const MODEL: &str = "ann-warm-single-flight-test-model";
        const DIMS: usize = 16;

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..16u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("ann single-flight note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        }

        let ann = new_shared();

        // Two concurrent callers warming the same model, mirroring boot warm
        // racing a recall-miss warm for the same key.
        let (r1, r2) = tokio::join!(
            ensure_ann_for_model(&rt, &token, &ann, MODEL),
            ensure_ann_for_model(&rt, &token, &ann, MODEL)
        );
        r1.expect("first caller must succeed");
        r2.expect("second caller must succeed");

        assert!(
            ann.indexes
                .read()
                .await
                .contains_key(&AnnKey::from_token(&token, MODEL)),
            "the model must end up warm regardless of which caller built it"
        );

        let store = rt.events(&token).expect("event store for local namespace");
        let page = store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::types::PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query_events");

        let started = page
            .items
            .iter()
            .filter(|e| e.kind == khive_types::EventKind::PhaseStarted)
            .count();
        let completed = page
            .items
            .iter()
            .filter(|e| e.kind == khive_types::EventKind::PhaseCompleted)
            .count();
        assert_eq!(
            started, 1,
            "exactly one caller must emit PhaseStarted for the same model, got: {page:?}"
        );
        assert_eq!(
            completed, 1,
            "exactly one caller must emit PhaseCompleted for the same model, got: {page:?}"
        );
    }

    // PR #583 (see the rationale comment on
    // ensure_ann_background): the remember-path warm must register as a
    // tracked background task, not a bare tokio::spawn, so daemon shutdown's
    // drain() waits for it. The only externally observable proof of that
    // wiring is track_background_task's own process-wide counter — mirrors
    // crates/khive-runtime/src/daemon.rs's
    // `track_background_task_count_returns_to_zero_after_completion`.
    //
    // `#[serial(background_tasks)]`: recall.rs's tests in this same crate
    // also drive track_background_task (the serve-ledger append) against the
    // identical process-wide counter; serializing this test under the same
    // group name avoids racing a concurrent increment/decrement from those.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ensure_ann_background_registers_a_tracked_task_not_a_bare_spawn() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let ann = new_shared();
        let model = "ann-warm-tracked-test-model";

        let before = khive_runtime::background_task_count();
        let started = ensure_ann_background(&rt, &token, &ann, model).await;
        assert!(
            started,
            "first call for a fresh key must start a background warm"
        );
        assert!(
            khive_runtime::background_task_count() > before,
            "track_background_task's counter must reflect the new warm \
             immediately after enqueue (the increment is synchronous), \
             proving ensure_ann_background is tracked rather than a bare \
             tokio::spawn invisible to drain()"
        );

        // Let the tracked task finish so it doesn't leak into another test's
        // counter snapshot.
        for _ in 0..200 {
            if khive_runtime::background_task_count() <= before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn is_benign_shutdown_cancellation_accepts_cancelled_join_error() {
        // A real cancelled JoinError, produced the same way tokio produces
        // one internally when spawn_blocking's task is aborted at runtime
        // teardown — not a synthetic stand-in.
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        let join_err = handle
            .await
            .expect_err("aborted task must yield a JoinError");
        assert!(
            join_err.is_cancelled(),
            "sanity: abort() must produce a cancelled JoinError"
        );

        let err = RuntimeError::Storage(StorageError::driver(
            khive_storage::StorageCapability::Vectors,
            "vec_count",
            join_err,
        ));
        assert!(
            is_benign_shutdown_cancellation(&err),
            "a cancelled JoinError boxed inside a Driver error must classify as benign"
        );
    }

    #[tokio::test]
    async fn is_benign_shutdown_cancellation_rejects_panicked_join_error() {
        // A JoinError from a genuine panic is a different failure mode than
        // cancellation (`is_cancelled()` is false for panics) and must not be
        // swallowed as benign.
        let handle = tokio::spawn(async { panic!("intentional panic for classification test") });
        let join_err = handle
            .await
            .expect_err("panicked task must yield a JoinError");
        assert!(
            join_err.is_panic(),
            "sanity: this JoinError must be a panic, not a cancellation"
        );

        let err = RuntimeError::Storage(StorageError::driver(
            khive_storage::StorageCapability::Vectors,
            "vec_count",
            join_err,
        ));
        assert!(
            !is_benign_shutdown_cancellation(&err),
            "a panicked (not cancelled) JoinError must not be classified as benign"
        );
    }

    #[test]
    fn is_benign_shutdown_cancellation_rejects_genuine_driver_error() {
        // A real backend failure (not a JoinError at all) must still WARN —
        // the predicate must not treat every Driver error as benign.
        let io_err = std::io::Error::other("disk full");
        let err = RuntimeError::Storage(StorageError::driver(
            khive_storage::StorageCapability::Vectors,
            "vec_count",
            io_err,
        ));
        assert!(
            !is_benign_shutdown_cancellation(&err),
            "a genuine driver error must never be classified as benign shutdown cancellation"
        );
    }

    #[test]
    fn is_benign_shutdown_cancellation_rejects_non_storage_error() {
        // Guards the outer match arm: a RuntimeError variant unrelated to
        // storage must never be misclassified as a benign cancellation.
        let err = RuntimeError::Internal("unrelated internal error".into());
        assert!(!is_benign_shutdown_cancellation(&err));
    }

    // ── #812: warming guard must release on every exit ─────────────────────

    struct HashVecService {
        dims: usize,
    }

    fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0001_0000_01b3);
        }
        let mut v = Vec::with_capacity(dims);
        let mut s = h;
        for _ in 0..dims {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
        }
        v
    }

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for HashVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: lattice_embed::EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
        }

        fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "hash-vec"
        }
    }

    struct HashVecProvider {
        model_name: String,
        dims: usize,
    }

    #[async_trait::async_trait]
    impl khive_runtime::EmbedderProvider for HashVecProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
            Ok(Arc::new(HashVecService { dims: self.dims }))
        }
    }

    fn test_runtime_with_hash_embedder(model: &str, dims: usize) -> KhiveRuntime {
        let tmp = tempfile::Builder::new()
            .prefix("khive-memory-ann-test-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp db dir");
        // Leaked deliberately: the runtime borrows this path only long enough
        // to open the SQLite file, and each test process only ever creates a
        // handful of these — matching this file's other embedder-backed
        // tests, which do the same via a per-test `tempfile::Builder` kept
        // alive by dropping it at the end of the enclosing scope. Since we
        // return only the runtime (not the tempdir guard) to keep the test
        // helper simple, leak the guard so the directory outlives the test.
        let db_path = tmp.path().join("khive-graph.db");
        std::mem::forget(tmp);
        let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..khive_runtime::RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(HashVecProvider {
            model_name: model.to_owned(),
            dims,
        });
        rt
    }

    /// Regression test (#812): warm
    /// succeeds, a write bumps the generation, the rebuild it triggers
    /// completes, a second write lands, and the model eventually reaches a
    /// fresh result — proving the `warming` guard from the FIRST warm does
    /// not permanently block every later rebuild attempt.
    ///
    /// Fail-on-revert: reverting `ensure_ann_background`'s cleanup back to
    /// "only remove the guard when nothing got loaded" leaves `warming`
    /// containing `key` after the first successful warm below, failing the
    /// first assertion; the second `ensure_ann_background` call would then
    /// also return `false` (guard still set) instead of `true`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ensure_ann_background_releases_warming_guard_after_success_and_allows_later_rebuild() {
        const MODEL: &str = "ann-warm-guard-release-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..4u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("warming guard note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(&token, MODEL);

        assert!(
            ensure_ann_background(&rt, &token, &ann, MODEL).await,
            "first call for a fresh key must start a background warm"
        );
        // Wait for the tracked task to fully exit (guard dropped), not merely
        // for the index to appear — the task still does async phase-event
        // bookkeeping after `install_if_fresher` and before returning, so
        // polling on index presence alone races the guard's release.
        for _ in 0..300 {
            if !ann.warming.lock().unwrap().contains(&key) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !ann.warming.lock().unwrap().contains(&key),
            "the warming guard must be released once the first background warm \
             finishes, not left set forever after a success (#812)"
        );
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "the first background warm must install an index"
        );

        // A second write lands: bump the generation exactly like
        // `memory.remember` does, then request another background warm.
        bump_generation(&ann, &key).await;
        assert!(
            ensure_ann_background(&rt, &token, &ann, MODEL).await,
            "a write landing after a completed warm must be able to schedule a \
             new background rebuild — if the guard were still set from the \
             first warm this would wrongly return false, and every later \
             recall would keep serving the now-stale index forever"
        );

        for _ in 0..300 {
            if is_current(&ann, &key).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            is_current(&ann, &key).await,
            "the second background warm must eventually install a fresh entry"
        );
    }

    // ── #812: content-hash restart validation ──────────────────────────────

    /// Regression test (#812), still valid under the content-hash redesign:
    /// a delete-one/add-one that preserves
    /// vector count and dimensions must still be detected as stale by a
    /// fresh `AnnState` (simulating a process restart, where write-generations
    /// reset to 0) so the model is rebuilt instead of silently serving the
    /// outdated snapshot forever.
    ///
    /// Fail-on-revert: reverting `ensure_ann_for_model_inner` to compare only
    /// `CorpusFingerprint` (vector count + dimensions) makes the second warm
    /// below take the Hot/`LoadedSnapshot` path instead of `Built`, because
    /// the corpus still has 4 vectors at the same dimensionality.
    #[tokio::test]
    async fn ensure_ann_for_model_restart_content_hash_mismatch_triggers_rebuild() {
        const MODEL: &str = "ann-warm-restart-signal-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let mut note_ids = Vec::new();
        for i in 0..4u32 {
            let note = rt
                .create_note_with_decay_for_embedding_model(
                    &token,
                    "memory",
                    None,
                    &format!("restart signal note {i}"),
                    Some(0.7),
                    0.01,
                    None,
                    vec![],
                    None,
                )
                .await
                .expect("create note");
            note_ids.push(note.id);
        }

        // First "process": warm and persist a snapshot over the initial
        // 4-note corpus.
        let ann1 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
            .await
            .expect("first warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "expected a fresh build over 4 vectors, got: {status:?}"
        );

        // Delete one note and add a fresh one: vector count and dimensions
        // both come back unchanged (still 4, still DIMS), but the corpus
        // content has moved on.
        assert!(
            rt.delete_note(&token, note_ids[0], false)
                .await
                .expect("soft delete"),
            "soft delete must succeed"
        );
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "restart signal note REPLACEMENT",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create replacement note");

        // "Restart": a fresh `AnnState` with generations reset to 0, exactly
        // like a process restart — write-generation tracking (#750) cannot
        // see this corpus change at all, so only restart validation against
        // the persisted snapshot can catch it.
        let ann2 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
            .await
            .expect("post-restart warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "a same-cardinality corpus content change must be detected as \
             stale and force a rebuild rather than silently loading the \
             outdated snapshot forever (#812), got: {status:?}"
        );
    }

    /// Regression test (#812): a
    /// vector-only re-embed — same note IDs, same count, same dimensions,
    /// same `notes.updated_at` (never touched) but different embedding
    /// bytes, exactly what `kkernel reindex` produces — must still be
    /// detected as stale by restart validation.
    ///
    /// Fail-on-revert: reverting `compute_corpus_content_hash` (or the
    /// build-side hash it's paired against) back to the old
    /// `CorpusContentSignal` (count + `MAX(notes.updated_at)`) makes the
    /// second warm below wrongly take the Hot/`LoadedSnapshot` path, because
    /// neither the count nor `updated_at` changed — only the embedding bytes
    /// did.
    #[tokio::test]
    async fn ensure_ann_for_model_restart_detects_vector_only_reindex() {
        const MODEL: &str = "ann-warm-restart-vector-only-reindex-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let mut note_ids = Vec::new();
        for i in 0..4u32 {
            let note = rt
                .create_note_with_decay_for_embedding_model(
                    &token,
                    "memory",
                    None,
                    &format!("vector-only reindex note {i}"),
                    Some(0.7),
                    0.01,
                    None,
                    vec![],
                    None,
                )
                .await
                .expect("create note");
            note_ids.push(note.id);
        }

        let ann1 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
            .await
            .expect("first warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "expected a fresh build over 4 vectors, got: {status:?}"
        );

        // Simulate `kkernel reindex`: overwrite the embedding blob for one
        // note directly in the vector table, bypassing
        // `create_note_with_decay_for_embedding_model` entirely so
        // `notes.updated_at` is never touched — same as reindex.rs's
        // `embed_and_store_batch` path, which writes vectors without
        // touching the notes table at all.
        {
            let table_name = format!("vec_{}", sanitize_model_key(MODEL));
            let replacement: Vec<f32> = (0..DIMS).map(|i| (i as f32 + 100.0) / 7.0).collect();
            let bytes: Vec<u8> = replacement.iter().flat_map(|f| f.to_le_bytes()).collect();
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: format!(
                    "UPDATE {table_name} SET embedding = ?1 \
                     WHERE subject_id = ?2 AND embedding_model = ?3"
                ),
                params: vec![
                    SqlValue::Blob(bytes),
                    SqlValue::Text(note_ids[0].to_string()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test_vector_only_reindex".into()),
            })
            .await
            .expect("overwrite embedding");
        }

        // "Restart": a fresh `AnnState`, generations reset to 0 — matches a
        // real restart exactly, and also matches `kkernel reindex` running
        // as a separate process from the daemon, which shares no in-memory
        // generation state with it at all.
        let ann2 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
            .await
            .expect("post-reindex warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "a vector-only re-embed that never touches notes.updated_at must \
             still be detected as stale and force a rebuild (#812), got: {status:?}"
        );
    }

    // ── #812: durable epoch vs. warm daemon ────────────────────────────────

    /// Regression test (#812): `kkernel reindex` runs as a SEPARATE process
    /// from a khive
    /// daemon that already warmed its ANN index — the two share a database
    /// file but nothing in-memory. Deleting the persisted snapshot row (as
    /// `invalidate_active_memory_vamana_snapshot` does) is invisible to the
    /// warm daemon's `common.rs` recall path, which only ever consults its
    /// own in-memory generation counter. This models exactly that scenario
    /// with two independent `KhiveRuntime`/`AnnState` pairs sharing one
    /// SQLite file, and asserts the amortized durable-epoch check
    /// (`maybe_check_durable_epoch`) is what closes the gap.
    ///
    /// Fail-on-revert: without `bump_memory_ann_epoch`/`maybe_check_durable_epoch`,
    /// `is_current` on `ann1` stays `true` forever after `rt2`'s reindex, and
    /// the final `ensure_ann_for_model` call returns `AlreadyLoaded` instead
    /// of `Built`.
    #[tokio::test]
    async fn maybe_check_durable_epoch_detects_reindex_from_a_separate_warm_daemon() {
        const MODEL: &str = "ann-warm-durable-epoch-test-model";
        const DIMS: usize = 8;

        let tmp = tempfile::Builder::new()
            .prefix("khive-memory-ann-durable-epoch-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp db dir");
        let db_path = tmp.path().join("khive-graph.db");
        std::mem::forget(tmp);

        // "Daemon": first runtime, warms the ANN index and stays resident —
        // exactly like a long-lived `kkernel mcp --daemon` process.
        let rt1 = KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(db_path.clone()),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..khive_runtime::RuntimeConfig::default()
        })
        .expect("runtime 1");
        rt1.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });
        let token1 = rt1.authorize(Namespace::local()).expect("authorize local");

        let mut note_ids = Vec::new();
        for i in 0..4u32 {
            let note = rt1
                .create_note_with_decay_for_embedding_model(
                    &token1,
                    "memory",
                    None,
                    &format!("durable epoch note {i}"),
                    Some(0.7),
                    0.01,
                    None,
                    vec![],
                    None,
                )
                .await
                .expect("create note");
            note_ids.push(note.id);
        }

        let ann1 = new_shared();
        let key = AnnKey::from_token(&token1, MODEL);
        let status = ensure_ann_for_model(&rt1, &token1, &ann1, MODEL)
            .await
            .expect("first warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "expected initial build, got: {status:?}"
        );

        // "Reindexer": a SEPARATE runtime pointed at the same DB file, like
        // `kkernel reindex` invoked while the daemon above stays warm.
        let rt2 = KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..khive_runtime::RuntimeConfig::default()
        })
        .expect("runtime 2");
        rt2.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        // Vector-only re-embed, bypassing the notes table entirely — same
        // shape as `reindex.rs`'s `embed_and_store_batch`.
        {
            let table_name = format!("vec_{}", sanitize_model_key(MODEL));
            let replacement: Vec<f32> = (0..DIMS).map(|i| (i as f32 + 100.0) / 7.0).collect();
            let bytes: Vec<u8> = replacement.iter().flat_map(|f| f.to_le_bytes()).collect();
            let sql = rt2.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: format!(
                    "UPDATE {table_name} SET embedding = ?1 \
                     WHERE subject_id = ?2 AND embedding_model = ?3"
                ),
                params: vec![
                    SqlValue::Blob(bytes),
                    SqlValue::Text(note_ids[0].to_string()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test_durable_epoch_vector_reindex".into()),
            })
            .await
            .expect("overwrite embedding");
        }
        // The reindexer deletes the persisted snapshot AND bumps the durable
        // epoch, exactly like `invalidate_active_memory_vamana_snapshot` +
        // `khive_pack_memory::bump_memory_ann_epoch` in `kkernel::reindex`.
        // `kkernel reindex` runs directly against a raw `KhiveRuntime` with
        // no pack registry boot (so `MemoryPack::SCHEMA_PLAN` is never
        // applied) — it calls `ensure_epoch_schema` explicitly once before
        // its first bump (see `begin_reindex_epoch` in `reindex.rs`); mirror
        // that here now that `bump_durable_epoch` no longer creates the
        // table itself (#812).
        ensure_epoch_schema(&rt2)
            .await
            .expect("ensure epoch schema");
        bump_durable_epoch(&rt2).await.expect("bump durable epoch");

        // Sanity: before the epoch check runs, the daemon's cache still
        // (wrongly) considers itself fresh — its in-memory generation was
        // never touched by `rt2`'s write.
        assert!(
            is_current(&ann1, &key).await,
            "sanity: the daemon's cache must still consider itself fresh before \
             the durable-epoch check runs"
        );
        maybe_check_durable_epoch(&rt1, &ann1, &key).await;
        assert!(
            !is_current(&ann1, &key).await,
            "the amortized durable-epoch check must detect a cross-process \
             reindex and mark the warm daemon's cached entry stale (#812)"
        );

        let status = ensure_ann_for_model(&rt1, &token1, &ann1, MODEL)
            .await
            .expect("rebuild after epoch mismatch");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "the warm daemon must rebuild once its durable-epoch check detects \
             the out-of-process reindex, got: {status:?}"
        );
    }

    // ── #812: high-water re-enqueue on drop ────────────────────────────────

    /// Regression test (#812): a write landing
    /// while a background warm is already in flight must eventually
    /// converge WITHOUT any later `memory.recall` or `memory.remember`
    /// re-triggering the warm — the in-flight task's own loop must notice
    /// the generation moved past what it built for and re-enqueue itself.
    ///
    /// Uses `attempt_floor_notify` as an explicit barrier instead of relying
    /// on a 300-note build being slow enough to still be in flight when
    /// `bump_generation` runs. That timing-based version did not provably
    /// exercise the re-enqueue path at all. The
    /// barrier guarantees the write lands strictly after the task has
    /// committed to its first attempt's generation floor, which is exactly
    /// the window the re-enqueue loop exists to cover.
    ///
    /// Fail-on-revert: reverting `ensure_ann_background`'s tracked task back
    /// to a single `ensure_ann_for_model` attempt (no loop) makes this test
    /// fail deterministically: nothing is left to notice the index fell
    /// behind after the barrier releases, and `is_current` never becomes
    /// true within the poll budget.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn ensure_ann_background_converges_on_write_during_warm_with_no_further_recalls() {
        const MODEL: &str = "ann-warm-medium-reenqueue-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..8u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("medium re-enqueue note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(&token, MODEL);

        // Arm the two-way barrier (#812): the
        // one-way `Notify` above only proved the task had captured its
        // floor, not that its build couldn't race ahead and read the
        // generation before this test's `bump_generation` below landed).
        // The task's first attempt now WAITS on `attempt_floor_release`
        // after emitting `attempt_floor_notify`, so the ordering here is
        // deterministic rather than a race against build speed.
        ann.attempt_floor_barrier
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let notified = ann.attempt_floor_notify.notified();
        assert!(
            ensure_ann_background(&rt, &token, &ann, MODEL).await,
            "first call for a fresh key must start a background warm"
        );
        // Wait for the tracked task to actually commit to its first
        // attempt's generation floor before bumping — this is the barrier
        // that replaces the old "300 notes should be slow enough" gamble.
        notified.await;
        // Simulate a write racing in while the warm above is still building —
        // bump the generation exactly like `memory.remember` does, but
        // deliberately do NOT call `ensure_ann_background` again: the whole
        // point is that no second caller ever arrives to notice or retrigger.
        bump_generation(&ann, &key).await;
        // Disarm BEFORE releasing so later attempts (attempt 2, 3, ...) in
        // this same task's loop don't also block waiting for a release this
        // test never sends again. `Notify::notify_one` synchronizes with
        // the waiter's wakeup, so the task observes `barrier == false` by
        // the time it re-checks on its next attempt.
        ann.attempt_floor_barrier
            .store(false, std::sync::atomic::Ordering::SeqCst);
        ann.attempt_floor_release.notify_one();

        for _ in 0..500 {
            if is_current(&ann, &key).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            is_current(&ann, &key).await,
            "a write racing in during an in-flight warm must eventually be \
             picked up and converge on its own, with zero further recalls or \
             writes to retrigger it (#812)"
        );
    }
}
