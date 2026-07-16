//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.
//! See `crates/khive-pack-memory/docs/api/ann-lifecycle.md` for lifecycle and race handling.

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
    /// Indexed namespaces, used to skip unnecessary recall over-fetch retries.
    pub(crate) namespace_set: HashSet<String>,
    /// In-process write generation captured before this build's corpus scan.
    pub(crate) generation: u64,
    /// Durable corpus epoch observed at build or snapshot load time.
    pub(crate) epoch_baseline: u64,
}

/// Shared model-index cache with single-flight and freshness coordination.
pub(crate) struct AnnState {
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    /// Synchronous so `WarmingGuard::drop` can release it on every exit path.
    warming: std::sync::Mutex<HashSet<AnnKey>>,
    /// Per-model warm lock shared by boot, background, and cold-recall paths.
    model_locks: Mutex<HashMap<AnnKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Monotonic per-model write generations used to reject stale installs.
    generations: Mutex<HashMap<AnnKey, u64>>,
    /// Last durable-epoch query per model, used to debounce warm-hit checks.
    last_epoch_check: std::sync::Mutex<HashMap<AnnKey, std::time::Instant>>,
    /// Counts how many times `search_loaded` returned a warm hit. Test-only;
    /// call `reset_warm_route_count()` between operations to isolate counts.
    #[cfg(test)]
    pub(crate) warm_route_count: AtomicUsize,
    /// Test barrier notified after a background attempt selects its generation floor.
    #[cfg(test)]
    pub(crate) attempt_floor_notify: tokio::sync::Notify,
    /// Test barrier that pauses the first attempt after floor selection.
    #[cfg(test)]
    pub(crate) attempt_floor_release: tokio::sync::Notify,
    /// Arms the test-only two-way floor-selection handshake.
    #[cfg(test)]
    pub(crate) attempt_floor_barrier: std::sync::atomic::AtomicBool,
    /// Test notification emitted when the background warming guard becomes idle.
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

/// Increment and return a model's generation without clearing its installed fallback.
///
/// See `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
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

/// Whether an installed graph satisfies the caller's minimum generation.
async fn installed_is_fresh(ann: &SharedAnn, key: &AnnKey, min_generation: u64) -> bool {
    ann.indexes
        .read()
        .await
        .get(key)
        .is_some_and(|b| b.generation >= min_generation)
}

/// Whether the installed graph covers the latest in-process write generation.
pub(crate) async fn is_current(ann: &SharedAnn, key: &AnnKey) -> bool {
    let target_generation = current_generation(ann, key).await;
    installed_is_fresh(ann, key, target_generation).await
}

/// Debounce interval for the cross-process durable-epoch query.
#[cfg(not(test))]
const DURABLE_EPOCH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const DURABLE_EPOCH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(0);

/// Delay between chained rebuild tasks so continuous writes coalesce.
#[cfg(not(test))]
const REBUILD_CHAIN_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
const REBUILD_CHAIN_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(5);

/// Mark a cached graph stale when its debounced durable epoch has advanced.
///
/// See `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
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

/// Pack-owned DDL for the durable ANN corpus epoch table.
pub(crate) const MEMORY_SCHEMA_PLAN_STMTS: [&str; 1] =
    ["CREATE TABLE IF NOT EXISTS memory_ann_epoch (\
     id INTEGER PRIMARY KEY CHECK (id = 1), \
     epoch INTEGER NOT NULL DEFAULT 0\
 )"];

/// Idempotently create the durable epoch table; never called from the hot read path.
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

/// Read the durable corpus epoch, returning zero when unavailable or absent.
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

/// Increment and return the durable epoch after persisted snapshot invalidation.
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

/// Install a graph unless the cache already holds an equal or newer generation.
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

/// Return a model's warm lock without holding the lock-map mutex across warming.
async fn model_warm_lock(ann: &SharedAnn, key: &AnnKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = ann.model_locks.lock().await;
    locks
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Holds a model's production warm lock so tests can create deterministic contention.
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

    /// Stamps the build with the write generation represented by its corpus snapshot.
    pub(crate) fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Stamps the build with the independently durable corpus epoch it observed.
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
            // Empty is conservative: recall assumes non-visible namespaces may exist.
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

/// Return the installed graph's namespaces; an empty set requires conservative retry.
pub(crate) async fn index_namespace_set(ann: &SharedAnn, key: &AnnKey) -> Option<HashSet<String>> {
    let guard = ann.indexes.read().await;
    guard.get(key).map(|b| b.namespace_set.clone())
}

/// Evict an unusable graph after ANN search failure; ordinary writes never call this.
pub(crate) async fn clear_key(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
    lock_warming(ann).remove(key);
}

/// Lock the synchronous warming set, recovering so one panic cannot disable future warms.
fn lock_warming(ann: &SharedAnn) -> std::sync::MutexGuard<'_, HashSet<AnnKey>> {
    ann.warming
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Releases the fire-once warming key on success, error, or panic.
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

/// Wait until a model's background rebuild chain is idle.
///
/// Registers notification before testing the predicate so releases cannot be missed.
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

// Runtime owns the shared typed shutdown-cancellation classifier.

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

    // Capture the generation before the fast path so the caller observes prior writes.
    let target_generation = current_generation(ann, &key).await;

    // Presence is insufficient: the installed generation must cover the caller's floor.
    if installed_is_fresh(ann, &key, target_generation).await {
        return false;
    }

    if !try_take_warming_guard(ann, &key) {
        return false;
    }

    // Tracking lets daemon shutdown drain the task; callers still pay only for enqueue.
    spawn_rebuild_task(rt.clone(), ann.clone(), model.to_owned(), key);
    true
}

/// Synchronously claim a warming key for initial or chained task spawning.
fn try_take_warming_guard(ann: &SharedAnn, key: &AnnKey) -> bool {
    let mut warming = lock_warming(ann);
    if warming.contains(key) {
        return false;
    }
    warming.insert(key.clone());
    true
}

/// Spawn a tracked rebuild chain; the caller MUST already hold the warming key.
///
/// This stays synchronous so chained re-enqueue creates an independent `Send` future.
fn spawn_rebuild_task(rt: KhiveRuntime, ann: SharedAnn, model: String, key: AnnKey) {
    spawn_rebuild_task_inner(rt, ann, model, key, false);
}

/// Spawn one rebuild task; only post-release chained tasks pay the debounce delay.
fn spawn_rebuild_task_inner(
    rt: KhiveRuntime,
    ann: SharedAnn,
    model: String,
    key: AnnKey,
    chained: bool,
) {
    // RAII ties release to every tracked-task exit path.
    let warming_guard = WarmingGuard {
        ann: ann.clone(),
        key: key.clone(),
    };
    khive_runtime::track_background_task(async move {
        if chained {
            tokio::time::sleep(REBUILD_CHAIN_DEBOUNCE).await;
        }
        // Recheck after each build because writes that found this guard occupied were not queued.
        // Bound attempts so continuous writes cannot retain the guard indefinitely; daemon drain
        // supplies the shutdown bound for any remaining debounced chain.
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
                // Armed tests pause here so their generation bump precedes the build.
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
                    // Runtime teardown cancellation is expected, not a backend failure.
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
                // No newer generation exists, so another attempt cannot make progress.
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
        // Release BEFORE the final generation check so a raced write can claim a new task.
        drop(warming_guard);
        if current_generation(&ann, &key).await > attempt_floor
            && try_take_warming_guard(&ann, &key)
        {
            // Chained re-enqueue is debounced rather than immediately respawned.
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

/// Restore or rebuild one model graph under a shared single-flight lock.
///
/// The actual attempt emits one best-effort `ann_warm` phase span. See
/// `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
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

    // Read generation BEFORE any fast path or corpus snapshot to close the write race.
    let target_generation = current_generation(ann, &key).await;

    // Fast path: no lock needed if already warm AND fresh enough.
    if installed_is_fresh(ann, &key, target_generation).await {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    let lock = model_warm_lock(ann, &key).await;
    let _single_flight_guard = lock.lock().await;

    // A concurrent caller may have satisfied our generation while we waited.
    if installed_is_fresh(ann, &key, target_generation).await {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    let phase_start = std::time::Instant::now();
    // Process CPU is cumulative, so phase attribution requires entry and exit snapshots.
    let cpu_start = khive_runtime::process_resource_usage();
    // RAII keeps `ann_warm` visible to health reporting on every exit path.
    let _phase_guard = khive_runtime::register_active_phase("ann_warm");
    // Corpus size is diagnostic; failure to count must not fail warming.
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

/// Append a best-effort ANN warm phase event without changing the warm result.
async fn emit_ann_warm_phase_event<P: serde::Serialize>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    kind: khive_types::EventKind,
    payload: P,
) {
    // A missing event store means auditing is unconfigured, not that warming failed.
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

/// Restore or rebuild after the caller has established its generation floor.
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

    // Stamp the epoch observed before this attempt; only a later reindex invalidates it.
    let target_epoch = durable_epoch(rt).await;

    // Snapshot restore requires both cheap count/dimensions and the ordered raw-row hash.
    // The hash detects same-cardinality replacement and vector-only reindex after restart.
    if let Some(persisted) = try_load_snapshot(rt, ns, model).await {
        let current_fp = compute_memory_fingerprint(rt, token, model).await;
        let fp_matches = current_fp.is_some_and(|fp| persisted.snapshot.fingerprint == fp);
        // Avoid the full hash scan when the cheap fingerprint already proves staleness.
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

    // The fingerprint sandwich bounds scan races; generation ordering closes the
    // later persistence/install window and prevents an older build from winning.
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
                // Persist the hash produced by this exact graph-input scan.
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
            // Downgrade here before background-warm handling sees the same cancellation.
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

/// Build a graph from live model vectors and hash the exact ordered rows consumed.
async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<(AnnBridge, CorpusContentHash)>, RuntimeError> {
    let store = match rt.vectors_for_model(token, model) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    // Preserve typed storage/join errors so shutdown cancellation remains classifiable.
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
    // Hash graph-input UUIDs and raw bytes in this same loop; vector-only reindex must differ.
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

    // SQ8 training and Vamana construction are CPU-bound; keep them off Tokio workers.
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

/// BLAKE3 freshness signal over ordered `(subject_id, raw_embedding)` rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CorpusContentHash([u8; 32]);

/// Snapshot wrapper coupling a graph with its corpus content hash.
/// Legacy bare snapshots fail decoding and self-heal through the rebuild path.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedMemorySnapshot {
    snapshot: VamanaSnapshot,
    content_hash: CorpusContentHash,
}

/// Recompute the ordered live-row hash for restart-only snapshot validation.
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

    // Writes preserve the installed graph and snapshot until a fresher build replaces them.

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

    /// `search_loaded` serves an installed stale graph rather than forcing an inline rebuild.
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

    // These deterministic tests pin generation compare-and-replace semantics directly.

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

    /// A strictly newer candidate replaces the installed older generation.
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

    /// Equal generations preserve the existing entry to avoid equivalent rebuild churn.
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

    /// An installed generation behind the write counter is not current.
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

    // Even an empty-corpus attempt must emit one complete phase pair.
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

    // Concurrent callers share one model warm and therefore one phase pair.
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

    // The process-wide counter proves daemon shutdown can drain the tracked warm.
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
        // Leak the guard so the returned runtime's database directory remains alive.
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

    /// A completed warm releases its guard so a later write can trigger another rebuild.
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

    /// Content hash detects same-cardinality corpus replacement after restart.
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

    /// Content hash detects vector-only re-embedding with unchanged note metadata.
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

        // Match reindex behavior by changing vector bytes without touching note metadata.
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

    /// A durable epoch exposes cross-process reindexing to a daemon's warm graph.
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
        // Reindex schema setup is explicit because no pack registry boot runs in that process.
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

    /// An in-flight warm re-enqueues itself when a later write advances its generation floor.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
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

        // The two-way barrier orders the write after the task captures its first floor.
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
