//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::StorageError;
use khive_vamana::{CorpusFingerprint, VamanaConfig, VamanaIndex, VamanaSnapshot};
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
}

/// Shared ANN state: per-`(namespace, model)` indexes with at-most-one-background-build guard.
pub(crate) struct AnnState {
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    warming: Mutex<HashSet<AnnKey>>,
    /// Review finding (issue #723 fix-round): per-model single-flight lock
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
    /// Counts how many times `search_loaded` returned a warm hit. Test-only;
    /// call `reset_warm_route_count()` between operations to isolate counts.
    #[cfg(test)]
    pub(crate) warm_route_count: AtomicUsize,
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(AnnState {
        indexes: RwLock::new(HashMap::new()),
        warming: Mutex::new(HashSet::new()),
        model_locks: Mutex::new(HashMap::new()),
        generations: Mutex::new(HashMap::new()),
        #[cfg(test)]
        warm_route_count: AtomicUsize::new(0),
    })
}

/// Bump `key`'s write-generation counter and return the NEW value (#750).
/// Called by `memory.remember` for every model whose corpus the write may
/// have affected, right alongside the existing cache invalidation.
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
pub(crate) async fn clear_key(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
    ann.warming.lock().await.remove(key);
}

/// Remove all in-memory ANN slots and warming-guard entries.
/// Because the index is global per model, any namespace write invalidates all slots.
pub(crate) async fn clear_namespace(ann: &SharedAnn, _namespace: &str) {
    ann.indexes.write().await.clear();
    ann.warming.lock().await.clear();
}

/// Clear in-memory cache and delete persisted snapshots.
pub(crate) async fn invalidate_namespace(rt: &KhiveRuntime, ann: &SharedAnn, _namespace: &str) {
    clear_namespace(ann, _namespace).await;
    invalidate_snapshots(rt).await;
}

/// True when `err` is the direct result of a `spawn_blocking` cancellation —
/// e.g. a short-lived process (or daemon shutdown) tearing the runtime down
/// mid-build — rather than a genuine backend/driver failure.
///
/// Matches the concrete `tokio::task::JoinError` boxed inside
/// `StorageError::Driver` (the shape `with_reader`/`with_writer` produce when
/// their `spawn_blocking(...).await` is cut short) via a typed downcast, not
/// a message substring, so a real `vec_count`/SQL driver error is never
/// misclassified as benign.
fn is_benign_shutdown_cancellation(err: &RuntimeError) -> bool {
    let RuntimeError::Storage(StorageError::Driver { source, .. }) = err else {
        return false;
    };
    source
        .downcast_ref::<tokio::task::JoinError>()
        .is_some_and(tokio::task::JoinError::is_cancelled)
}

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

    // Single-flight guard.
    {
        let mut warming = ann.warming.lock().await;
        if warming.contains(&key) {
            return false;
        }
        warming.insert(key.clone());
    }

    let rt = rt.clone();
    let ann = ann.clone();
    let model = model.to_owned();
    // Tracked, not a bare tokio::spawn, so daemon shutdown's drain() waits for
    // an in-flight remember-path warm instead of a SIGTERM (or a short-lived
    // `kkernel exec` process exiting) aborting it mid-build — same rationale
    // as recall.rs's serve-ledger append (internal review PR #583 round-1
    // Medium). The caller still only pays for the enqueue; the build itself
    // runs fully off the response path, unawaited.
    khive_runtime::track_background_task(async move {
        if let Ok(token) = rt.authorize(Namespace::local()) {
            match ensure_ann_for_model(&rt, &token, &ann, &model).await {
                Ok(status) => {
                    tracing::debug!(?status, model = %model, "memory ANN background warm complete");
                }
                Err(e) if is_benign_shutdown_cancellation(&e) => {
                    // Expected on a short-lived process: the build's
                    // spawn_blocking was cancelled by runtime teardown, not a
                    // backend failure — don't alarm on it.
                    tracing::debug!(error = %e, model = %model, "memory ANN background warm cancelled at shutdown");
                }
                Err(e) => {
                    tracing::warn!(error = %e, model = %model, "memory ANN background build failed");
                }
            }
        }
        // If loading/building failed, remove the guard so a later recall retries.
        let loaded = ann.indexes.read().await.contains_key(&key);
        if !loaded {
            ann.warming.lock().await.remove(&key);
        }
    });
    true
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
/// Review finding (issue #723 fix-round): because three independent call
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
    // Review finding (issue #723 fix-round): `process_resource_usage()`
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

    // Try snapshot warm-load.
    if let Some(snapshot) = try_load_snapshot(rt, ns, model).await {
        let current_fp = compute_memory_fingerprint(rt, token, model).await;
        if let Some(fp) = current_fp {
            if snapshot.fingerprint == fp {
                match AnnBridge::from_snapshot(snapshot) {
                    Ok(mut bridge) => {
                        // Populate namespace set from a cheap DISTINCT query so the
                        // retry gate in recall can short-circuit when appropriate.
                        let ns_set = query_distinct_namespaces(rt, token, model)
                            .await
                            .unwrap_or_default();
                        bridge.set_namespace_set(ns_set);
                        let bridge = bridge.with_generation(target_generation);
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
                    "stale memory Vamana snapshot (fingerprint mismatch); rebuilding"
                );
            }
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
        Ok(Some(bridge)) => {
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
            let bridge = bridge.with_generation(target_generation);
            if let Some(fingerprint) = fp_after {
                if let Err(e) = persist_snapshot(rt, ns, model, &bridge, fingerprint).await {
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

/// Scan all non-deleted `note.content` vectors across all namespaces for `model` and build an
/// `AnnBridge`. Returns `Ok(None)` when the corpus is empty or inaccessible.
async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
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
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }

    if id_map.is_empty() {
        return Ok(None);
    }

    AnnBridge::build(flat, dims, id_map, namespace_set).map(Some)
}

// ── persistence ───────────────────────────────────────────────────────────────

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
) -> Result<(), RuntimeError> {
    if let Err(e) = ensure_snapshot_schema(rt).await {
        tracing::warn!(error = %e, "failed to create retrieval_snapshots schema");
        return Err(e);
    }
    let snapshot = bridge
        .to_snapshot(namespace, model, fingerprint)
        .map_err(|e| RuntimeError::Internal(format!("to_snapshot: {e}")))?;
    let blob = serde_json::to_vec(&snapshot)
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
) -> Option<VamanaSnapshot> {
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
    match serde_json::from_slice::<VamanaSnapshot>(&blob) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "corrupt memory Vamana snapshot blob");
            None
        }
    }
}

/// Delete the global memory Vamana snapshot rows from `retrieval_snapshots`.
/// Best-effort — missing table is silently ignored.
async fn invalidate_snapshots(rt: &KhiveRuntime) {
    let pattern = "global::memory_vamana::%".to_string();
    let sql = rt.sql();
    let mut w = match sql.writer().await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open writer for memory ANN snapshot invalidation");
            return;
        }
    };
    match w
        .execute(SqlStatement {
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1".into(),
            params: vec![SqlValue::Text(pattern)],
            label: Some("invalidate_memory_vamana_snapshot".into()),
        })
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("no such table") => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to invalidate memory Vamana snapshots");
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

    #[tokio::test]
    async fn invalidate_namespace_clears_global_index() {
        // After FTS+ANN consolidation, AnnKey is model-only (namespace is ignored).
        // invalidate_namespace clears the single global index for all models.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let model_a = "model-a";
        let model_b = "model-b";

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        let bridge_a = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![id_a], HashSet::new())
            .expect("build a");
        let bridge_b = AnnBridge::build(vec![0.0f32, 1.0, 0.0, 0.0], 4, vec![id_b], HashSet::new())
            .expect("build b");

        // Keys are model-only; namespace arg is ignored.
        let key_a = AnnKey::new("any-ns", model_a);
        let key_b = AnnKey::new("any-ns", model_b);

        {
            let mut idxs = ann.indexes.write().await;
            idxs.insert(key_a.clone(), bridge_a);
            idxs.insert(key_b.clone(), bridge_b);
        }
        {
            let mut warming = ann.warming.lock().await;
            warming.insert(key_a.clone());
            warming.insert(key_b.clone());
        }

        // invalidate_namespace evicts ALL in-memory indexes (global index serves all namespaces).
        invalidate_namespace(&rt, &ann, "any-ns").await;

        assert!(
            ann.indexes.read().await.is_empty(),
            "all indexes must be cleared after invalidation"
        );
        assert!(
            ann.warming.lock().await.is_empty(),
            "all warming guards must be cleared after invalidation"
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

    // Review finding (issue #723 fix-round): two concurrent callers warming
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

    // internal review PR #583 round-1 Medium (see the rationale comment on
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
}
