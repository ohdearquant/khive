//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.
//! See `crates/khive-pack-memory/docs/api/ann-lifecycle.md` for lifecycle and race handling,
//! and `crates/khive-pack-memory/docs/ann.md` for the restart classifier and ADR-118 design.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_runtime::ann_registry::{self, CompactionScope, WatermarkAuthority, PENDING_WATERMARK};
use khive_runtime::config::ann_rebuild_threshold_from_env as ann_rebuild_threshold;
use khive_runtime::{
    is_benign_shutdown_cancellation, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::StorageError;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{
    read_commit_fingerprint, read_commit_info, read_external_ids_sidecar, segment_commit_digest,
    write_external_ids_sidecar, CorpusFingerprint, VamanaConfig, VamanaIndex,
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[cfg(test)]
tokio::task_local! {
    static SESSION_EXACT_STATEMENT_COUNT: std::cell::Cell<usize>;
    static SESSION_FENCE_PROBE_COUNT: std::cell::Cell<usize>;
}

#[cfg(test)]
pub(crate) async fn count_session_statements<F: std::future::Future>(
    future: F,
) -> (F::Output, usize, usize) {
    SESSION_EXACT_STATEMENT_COUNT
        .scope(std::cell::Cell::new(0), async {
            SESSION_FENCE_PROBE_COUNT
                .scope(std::cell::Cell::new(0), async {
                    let output = future.await;
                    let probes = SESSION_FENCE_PROBE_COUNT.with(std::cell::Cell::get);
                    let exact = SESSION_EXACT_STATEMENT_COUNT.with(std::cell::Cell::get);
                    (output, probes, exact)
                })
                .await
        })
        .await
}

#[path = "ann/incremental.rs"]
mod incremental;
use incremental::*;
#[path = "ann/checkpoint_timer.rs"]
mod checkpoint_timer;
#[path = "ann/delta.rs"]
mod delta;

// ── types ─────────────────────────────────────────────────────────────────────

/// Cache key for a per-model ANN slot (one index per model, all namespaces combined).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct AnnKey {
    pub(crate) model: String,
}

impl AnnKey {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }

    pub(crate) fn from_token(model: &str) -> Self {
        Self {
            model: model.to_owned(),
        }
    }
}

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    incarnation: Arc<()>,
    #[cfg(test)]
    reverse_map_scan_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    id_map: Vec<Uuid>,
    /// Built on first replay; subsequent batches update only changed subjects.
    reverse_map: Option<HashMap<Uuid, u32>>,
    #[cfg(test)]
    reverse_map_builds: usize,
    dirty_ops: u64,
    published_seq: u64,
    last_checkpoint: std::time::Instant,
    /// Digest of the v2 commit record this mmap bridge loaded. Every
    /// file-backed publication carries a fresh nonce, so equality means the
    /// mapped file generation is still current (#2081). Owned builds have no
    /// publication identity until they are persisted and reopened.
    commit_digest: Option<[u8; 32]>,
    /// The stable v2 commit remains unchanged while memory-owned delta
    /// publications advance their own nonce and watermark beside it.
    base_commit_digest: Option<[u8; 32]>,
    base_applied_seq: u64,
    base_ops: usize,
    delta_batches: Vec<delta::DeltaBatch>,
    delta_raw_ops: u64,
    delta_chunks: usize,
    last_delta_nonce: Option<Uuid>,
    /// Indexed namespaces, used to skip unnecessary recall over-fetch retries.
    pub(crate) namespace_set: HashSet<String>,
    /// In-process write generation captured before this build's corpus scan.
    pub(crate) generation: u64,
    /// Durable corpus epoch observed at build or snapshot load time.
    pub(crate) epoch_baseline: u64,
    /// Lets rotation tests prove replacement drops the bridge that owns the
    /// predecessor mmaps, rather than merely changing the cache metadata.
    #[cfg(test)]
    drop_probe: Option<Arc<()>>,
}

#[derive(Clone, Copy)]
pub(crate) enum AnnScoreRoute {
    Memory,
    NoteSearch,
}

#[derive(Clone, Copy)]
pub(crate) struct FreshTailSearch<'a> {
    query: &'a [f32],
    k: usize,
    route: AnnScoreRoute,
}

impl<'a> FreshTailSearch<'a> {
    pub(crate) fn new(query: &'a [f32], k: usize, route: AnnScoreRoute) -> Self {
        Self { query, k, route }
    }
}

impl AnnScoreRoute {
    fn graph_score(self, squared_l2_distance: f32) -> Result<f64, RuntimeError> {
        match self {
            Self::Memory => Ok(f64::from((1.0 - squared_l2_distance / 2.0).max(0.0))),
            Self::NoteSearch => khive_score::try_cosine_score_with_f32_tolerance(
                f64::from(squared_l2_distance) / 2.0,
            )
            .map(|score| score.to_f64())
            .map_err(|error| {
                RuntimeError::Internal(format!("note-search ANN cosine distance: {error}"))
            }),
        }
    }

    fn tail_score(self, query: &[f32], embedding: &[f32]) -> Result<f64, RuntimeError> {
        let cosine = exact_cosine_unclamped(query, embedding);
        match self {
            Self::Memory => Ok(f64::from(cosine.max(0.0))),
            Self::NoteSearch => {
                khive_score::try_cosine_score_with_f32_tolerance(1.0 - f64::from(cosine))
                    .map(|score| score.to_f64())
                    .map_err(|error| {
                        RuntimeError::Internal(format!(
                            "note-search fresh-tail cosine distance: {error}"
                        ))
                    })
            }
        }
    }
}

#[cfg(test)]
#[test]
fn note_search_score_carrier_preserves_non_cardinal_fixed_point_bits() {
    let squared_l2_distance = 0.41_f32;
    let canonical =
        khive_score::try_cosine_score_with_f32_tolerance(f64::from(squared_l2_distance) / 2.0)
            .expect("canonical cosine score");
    let carried = AnnScoreRoute::NoteSearch
        .graph_score(squared_l2_distance)
        .expect("note-search score");
    assert_eq!(
        khive_score::DeterministicScore::from_f64(carried),
        canonical
    );
    assert_ne!(
        khive_score::DeterministicScore::from_f64(f64::from(carried as f32)),
        canonical,
        "fixture must detect a lossy f32 candidate-score roundtrip"
    );
}

/// Shared model-index cache with single-flight and freshness coordination.
pub(crate) struct AnnState {
    /// Whether this process may build the memory index from the full corpus and
    /// publish the result. A corpus build is minutes of CPU and a segment
    /// rewrite every other reader on the index root must then absorb, and it
    /// pays for itself only in a process that outlives the request. Serving
    /// processes set this from the daemon role at construction; the admin
    /// reindex path sets it unconditionally, because building is what it was
    /// invoked to do.
    pub(crate) builds_corpus_indexes: bool,
    /// Set when the registered memory pack supplies this graph to note search.
    /// Direct ANN fixtures without the provider retain the memory-only lifecycle.
    note_search_consumer_enabled: AtomicBool,
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    checkpoint_policy: std::sync::RwLock<CheckpointPolicy>,
    checkpoint_timers: std::sync::Mutex<HashSet<AnnKey>>,
    checkpoint_timers_enabled: bool,
    #[cfg(test)]
    segment_load_count: AtomicUsize,
    #[cfg(test)]
    fail_next_segment_load: AtomicBool,
    #[cfg(test)]
    publication_count: AtomicUsize,
    /// Synchronous so `WarmingGuard::drop` can release it on every exit path.
    warming: std::sync::Mutex<HashSet<AnnKey>>,
    /// Per-model warm lock shared by boot, background, and cold-recall paths.
    model_locks: Mutex<HashMap<AnnKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Monotonic per-model write generations used to reject stale installs.
    generations: Mutex<HashMap<AnnKey, u64>>,
    /// Last durable-epoch query per model, used to debounce warm-hit checks.
    last_epoch_check: std::sync::Mutex<HashMap<AnnKey, std::time::Instant>>,
    /// Idempotence guard for the pack-lifetime file-generation watcher.
    rotation_watch_started: AtomicBool,
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
    /// Notified when a pathless fresh-tail reader waits for pending publication.
    #[cfg(test)]
    pub(crate) pathless_pending_publication_wait: tokio::sync::Notify,
    /// Arms a pause before a pathless incremental checkpoint publishes its watermark.
    #[cfg(test)]
    pub(crate) pathless_checkpoint_barrier: std::sync::atomic::AtomicBool,
    /// Notified when the pathless incremental checkpoint reaches the armed pause.
    #[cfg(test)]
    pub(crate) pathless_checkpoint_notify: tokio::sync::Notify,
    /// Releases the armed pathless incremental checkpoint pause.
    #[cfg(test)]
    pub(crate) pathless_checkpoint_release: tokio::sync::Notify,
    /// Signals that a pathless checkpoint is about to request its SQL writer.
    #[cfg(test)]
    pub(crate) pathless_watermark_attempt_notify: tokio::sync::Notify,
    /// Pauses after the pathless SQL watermark rises but before bridge publication.
    #[cfg(test)]
    pub(crate) pathless_post_watermark_barrier: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) pathless_post_watermark_notify: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) pathless_post_watermark_release: tokio::sync::Notify,
    /// Signals that mismatch recovery released its SQL snapshot before waiting.
    #[cfg(test)]
    pub(crate) pathless_reresolve_wait_notify: tokio::sync::Notify,
    /// Test barrier after the single-statement incremental-tail read returns.
    #[cfg(test)]
    pub(crate) protected_tail_barrier: std::sync::atomic::AtomicBool,
    /// Notified when incremental-tail maintenance reaches the post-read pause.
    #[cfg(test)]
    pub(crate) protected_tail_notify: tokio::sync::Notify,
    /// Releases the incremental-tail post-read pause.
    #[cfg(test)]
    pub(crate) protected_tail_release: tokio::sync::Notify,
    /// Test seam between the classifier's branch count and protected replay.
    #[cfg(test)]
    pub(crate) stale_tail_scope_barrier: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) stale_tail_scope_notify: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) stale_tail_scope_release: tokio::sync::Notify,
    /// Arms the test-only pause in `fresh_tail_reresolve` between its
    /// segment load and its registry-minimum re-check.
    #[cfg(test)]
    pub(crate) reresolve_race_barrier: std::sync::atomic::AtomicBool,
    /// Notified once `fresh_tail_reresolve` reaches the armed pause point.
    #[cfg(test)]
    pub(crate) reresolve_race_notify: tokio::sync::Notify,
    /// `fresh_tail_reresolve` waits on this to resume past the armed pause.
    #[cfg(test)]
    pub(crate) reresolve_race_release: tokio::sync::Notify,
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn enable_note_search_consumer(ann: &SharedAnn) {
    ann.note_search_consumer_enabled
        .store(true, Ordering::Release);
}

fn note_search_consumer_enabled(ann: &SharedAnn) -> bool {
    ann.note_search_consumer_enabled.load(Ordering::Acquire)
}

/// Shared ANN state for a process that builds corpus indexes. Test-only here:
/// production reaches this through `MemoryPack::new_with_index_role`, which
/// states the role rather than assuming it.
#[cfg(test)]
pub(crate) fn new_shared() -> SharedAnn {
    new_shared_for_role(true)
}

/// Shared ANN state whose corpus-build authority is stated explicitly. The
/// serving pack passes the daemon role; see `AnnState::builds_corpus_indexes`.
pub(crate) fn new_shared_for_role(builds_corpus_indexes: bool) -> SharedAnn {
    Arc::new(AnnState {
        builds_corpus_indexes,
        note_search_consumer_enabled: AtomicBool::new(false),
        indexes: RwLock::new(HashMap::new()),
        checkpoint_policy: std::sync::RwLock::new(CheckpointPolicy::from_env()),
        checkpoint_timers: std::sync::Mutex::new(HashSet::new()),
        checkpoint_timers_enabled: cfg!(not(test)),
        #[cfg(test)]
        segment_load_count: AtomicUsize::new(0),
        #[cfg(test)]
        fail_next_segment_load: AtomicBool::new(false),
        #[cfg(test)]
        publication_count: AtomicUsize::new(0),
        warming: std::sync::Mutex::new(HashSet::new()),
        model_locks: Mutex::new(HashMap::new()),
        generations: Mutex::new(HashMap::new()),
        last_epoch_check: std::sync::Mutex::new(HashMap::new()),
        rotation_watch_started: AtomicBool::new(false),
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
        #[cfg(test)]
        pathless_pending_publication_wait: tokio::sync::Notify::new(),
        #[cfg(test)]
        pathless_checkpoint_barrier: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        pathless_checkpoint_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        pathless_checkpoint_release: tokio::sync::Notify::new(),
        #[cfg(test)]
        pathless_watermark_attempt_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        pathless_post_watermark_barrier: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        pathless_post_watermark_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        pathless_post_watermark_release: tokio::sync::Notify::new(),
        #[cfg(test)]
        pathless_reresolve_wait_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        protected_tail_barrier: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        protected_tail_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        protected_tail_release: tokio::sync::Notify::new(),
        #[cfg(test)]
        stale_tail_scope_barrier: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        stale_tail_scope_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        stale_tail_scope_release: tokio::sync::Notify::new(),
        #[cfg(test)]
        reresolve_race_barrier: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        reresolve_race_notify: tokio::sync::Notify::new(),
        #[cfg(test)]
        reresolve_race_release: tokio::sync::Notify::new(),
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

/// Read the monotonic write generation so mutation-hook tests can assert the
/// invalidation signal without racing the background rebuild it schedules.
#[cfg(test)]
pub(crate) async fn generation_for_test(ann: &SharedAnn, key: &AnnKey) -> u64 {
    current_generation(ann, key).await
}

/// Debounce interval for the cross-process durable-epoch query.
#[cfg(not(test))]
const DURABLE_EPOCH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const DURABLE_EPOCH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(0);

/// Delay between chained rebuild tasks so continuous writes coalesce.
///
/// One second coalesces nothing against a fleet that writes continuously: the
/// chain re-enqueues before the next write arrives, so it never idles and the
/// index is rebuilt and republished on a cadence set by nothing in particular.
/// The chain exists so a write converges without a reader, not to keep readers
/// fresh — a recall warms on demand at request time — so the window it should
/// use is the one that batches a burst of writes into one build. Override with
/// `KHIVE_ANN_REBUILD_DEBOUNCE_MS`; a malformed value falls back to the default.
#[cfg(not(test))]
const REBUILD_CHAIN_DEBOUNCE_DEFAULT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const REBUILD_CHAIN_DEBOUNCE_DEFAULT: std::time::Duration = std::time::Duration::from_millis(5);

fn rebuild_chain_debounce() -> std::time::Duration {
    resolve_rebuild_chain_debounce(
        std::env::var("KHIVE_ANN_REBUILD_DEBOUNCE_MS")
            .ok()
            .as_deref(),
        REBUILD_CHAIN_DEBOUNCE_DEFAULT,
    )
}

/// Pure half of [`rebuild_chain_debounce`], so the policy is testable without
/// mutating process environment. Zero is a legal override: it means the caller
/// asked for no coalescing at all.
fn resolve_rebuild_chain_debounce(
    override_value: Option<&str>,
    default: std::time::Duration,
) -> std::time::Duration {
    match override_value.and_then(|raw| raw.trim().parse::<u64>().ok()) {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => default,
    }
}

/// File-generation polling cadence for mmap bridges. Only the tiny commit
/// record is read on an unchanged tick; vector/graph files are reopened only
/// after a distinct publication identity appears.
const ROTATION_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

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

    pub(crate) async fn pause_pathless_checkpoint_for_test(&self) {
        if self
            .pathless_checkpoint_barrier
            .swap(false, Ordering::SeqCst)
        {
            self.pathless_checkpoint_notify.notify_one();
            self.pathless_checkpoint_release.notified().await;
        }
    }

    pub(crate) async fn pause_protected_tail_for_test(&self) {
        if self.protected_tail_barrier.swap(false, Ordering::SeqCst) {
            self.protected_tail_notify.notify_one();
            self.protected_tail_release.notified().await;
        }
    }

    pub(crate) async fn pause_stale_tail_scope_for_test(&self) {
        if self.stale_tail_scope_barrier.swap(false, Ordering::SeqCst) {
            self.stale_tail_scope_notify.notify_one();
            self.stale_tail_scope_release.notified().await;
        }
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
        let index = VamanaIndex::build_owned(vectors, cfg)
            .map_err(|e| RuntimeError::Internal(e.to_string()))?;
        Ok(Self {
            index,
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: None,
            id_map,
            reverse_map: None,
            #[cfg(test)]
            reverse_map_builds: 0,
            dirty_ops: 0,
            published_seq: 0,
            last_checkpoint: std::time::Instant::now(),
            commit_digest: None,
            base_commit_digest: None,
            base_applied_seq: 0,
            base_ops: n,
            delta_batches: Vec::new(),
            delta_raw_ops: 0,
            delta_chunks: 0,
            last_delta_nonce: None,
            namespace_set,
            generation: 0,
            epoch_baseline: 0,
            #[cfg(test)]
            drop_probe: None,
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

    pub(crate) fn search_with_route(
        &self,
        query: &[f32],
        k: usize,
        route: AnnScoreRoute,
    ) -> Result<Vec<(Uuid, f64)>, RuntimeError> {
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        let raw = self
            .index
            .search(&q, k)
            .map_err(|e| RuntimeError::Internal(format!("memory ANN search: {e}")))?;
        let mut hits = Vec::with_capacity(raw.len());
        for (idx, dist) in raw {
            if let Some(uuid) = self.id_map.get(idx as usize) {
                // The graph emits normalized-vector L2². For note search,
                // L2²/2 is cosine distance and uses the same converter as the
                // exact sqlite-vec route; memory recall retains its prior floor.
                hits.push((*uuid, route.graph_score(dist)?));
            }
        }
        Ok(hits)
    }

    #[cfg(test)]
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(Uuid, f64)>, RuntimeError> {
        self.search_with_route(query, k, AnnScoreRoute::Memory)
    }

    /// Stamp the ann_write_log watermark this bridge's corpus state reflects
    /// (ADR-079 Amendment 1). Persisted by `save_atomic` into the extended
    /// commit record.
    pub(crate) fn set_applied_seq(&mut self, seq: u64) {
        self.index.set_last_applied_seq(Some(seq));
        self.published_seq = seq;
    }

    pub(crate) fn mark_checkpointed(&mut self) {
        self.dirty_ops = 0;
        self.published_seq = self.index.last_applied_seq().unwrap_or(0);
        self.last_checkpoint = std::time::Instant::now();
    }

    /// A full save can succeed even when the immediate mmap re-adoption fails.
    /// The retained owned bridge represents that exact durable base and must
    /// carry its identity before another incremental delta can be published.
    fn mark_full_checkpoint_base(&mut self, digest: [u8; 32]) {
        self.commit_digest = Some(digest);
        self.base_commit_digest = Some(digest);
        self.base_applied_seq = self.index.last_applied_seq().unwrap_or(0);
        self.base_ops = self.index.num_vectors();
        self.delta_batches.clear();
        self.delta_raw_ops = 0;
        self.delta_chunks = 0;
        self.last_delta_nonce = None;
    }

    fn mark_delta_checkpoint(&mut self, publication: &delta::DeltaPublication) {
        self.commit_digest = Some(publication.identity);
        self.last_delta_nonce = Some(publication.last_nonce);
        self.delta_chunks = publication.chunk_count;
        self.delta_batches.clear();
    }

    fn record_delta_batch(
        &mut self,
        ops: Vec<(Uuid, Option<Vec<f32>>)>,
        applied_seq: u64,
        raw_count: u64,
    ) {
        if raw_count == 0 {
            return;
        }
        self.delta_batches.push(delta::DeltaBatch {
            applied_seq,
            raw_count,
            ops,
        });
        self.delta_raw_ops = self.delta_raw_ops.saturating_add(raw_count);
    }

    fn needs_full_compaction(&self) -> bool {
        self.needs_full_compaction_with_chain_limit(delta::MAX_RETIRED_CHAIN)
    }

    fn needs_full_compaction_with_chain_limit(&self, max_chunks: usize) -> bool {
        self.delta_batches.is_empty()
            || self.delta_raw_ops >= delta::compaction_limit(self.base_ops)
            || self.delta_chunks >= max_chunks
    }

    fn fork_for_maintenance(&self) -> Self {
        Self {
            index: self.index.fork_for_maintenance(),
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: self.reverse_map_scan_hook.clone(),
            id_map: self.id_map.clone(),
            reverse_map: self.reverse_map.clone(),
            #[cfg(test)]
            reverse_map_builds: self.reverse_map_builds,
            dirty_ops: self.dirty_ops,
            published_seq: self.published_seq,
            last_checkpoint: self.last_checkpoint,
            commit_digest: self.commit_digest,
            base_commit_digest: self.base_commit_digest,
            base_applied_seq: self.base_applied_seq,
            base_ops: self.base_ops,
            delta_batches: self.delta_batches.clone(),
            delta_raw_ops: self.delta_raw_ops,
            delta_chunks: self.delta_chunks,
            last_delta_nonce: self.last_delta_nonce,
            namespace_set: self.namespace_set.clone(),
            generation: self.generation,
            epoch_baseline: self.epoch_baseline,
            #[cfg(test)]
            drop_probe: None,
        }
    }

    fn rebuild_reverse_map(&mut self) {
        // A tombstoned ordinal can retain its previous UUID until slot reuse.
        let mut reverse = HashMap::with_capacity(self.index.live_count());
        for (ordinal, uuid) in self.id_map.iter().enumerate() {
            #[cfg(test)]
            if ordinal == self.id_map.len() / 2 {
                if let Some(hook) = &self.reverse_map_scan_hook {
                    hook();
                }
            }
            if !self.index.is_tombstoned(ordinal as u32) {
                reverse.insert(*uuid, ordinal as u32);
            }
        }
        self.reverse_map = Some(reverse);
        #[cfg(test)]
        {
            self.reverse_map_builds += 1;
        }
    }

    /// Apply a coalesced final-state tail (ADR-079 Amendment 1) to this
    /// bridge: `Some(embedding)` replays a final upsert; `None` replays a
    /// final delete. `new_s` is stamped as the new applied watermark. A
    /// delete whose ordinal was reassigned by an earlier same-batch upsert
    /// is skipped with a warning, not an error; any other id-map
    /// contradiction returns `Err` (caller escalates to Cold). See
    /// `docs/ann.md` for the ownership-rule rationale (#1150).
    pub(crate) fn apply_final_ops(
        &mut self,
        ops: Vec<(Uuid, Option<Vec<f32>>)>,
        new_s: u64,
    ) -> Result<(), String> {
        if self.reverse_map.is_none() {
            self.rebuild_reverse_map();
        }
        let reverse = self.reverse_map.as_mut().expect("initialized reverse map");

        for (uuid, op) in ops {
            match op {
                None => {
                    if let Some(&ordinal) = reverse.get(&uuid) {
                        // Fail closed: if a same-batch upsert already reused
                        // this slot, skip the tombstone instead of deleting
                        // someone else's live vector.
                        if self.id_map.get(ordinal as usize) != Some(&uuid) {
                            tracing::warn!(
                                subject = %uuid,
                                ordinal,
                                "replay delete: ordinal reassigned within batch, skipping tombstone"
                            );
                            reverse.remove(&uuid);
                            continue;
                        }
                        self.index
                            .tombstone(ordinal)
                            .map_err(|e| format!("replay tombstone({ordinal}): {e}"))?;
                        reverse.remove(&uuid);
                    }
                }
                Some(mut embedding) => {
                    l2_normalize(&mut embedding);
                    if let Some(&old) = reverse.get(&uuid) {
                        if self.id_map.get(old as usize) != Some(&uuid) {
                            return Err(format!(
                                "replay upsert: ordinal {old} is no longer owned by {uuid}"
                            ));
                        }
                        self.index
                            .tombstone(old)
                            .map_err(|e| format!("replay tombstone({old}): {e}"))?;
                        reverse.remove(&uuid);
                    }
                    let ordinal = self
                        .index
                        .insert(&embedding)
                        .map_err(|e| format!("replay insert: {e}"))?;
                    let slot = ordinal as usize;
                    match slot.cmp(&self.id_map.len()) {
                        std::cmp::Ordering::Less => {
                            let previous_owner = self.id_map[slot];
                            if reverse.get(&previous_owner) == Some(&ordinal) {
                                reverse.remove(&previous_owner);
                            }
                            self.id_map[slot] = uuid;
                        }
                        std::cmp::Ordering::Equal => self.id_map.push(uuid),
                        std::cmp::Ordering::Greater => {
                            return Err(format!(
                                "replay insert returned ordinal {ordinal} beyond id_map len {}",
                                self.id_map.len()
                            ));
                        }
                    }
                    reverse.insert(uuid, ordinal);
                }
            }
        }
        self.index.set_last_applied_seq(Some(new_s));
        Ok(())
    }

    pub(crate) fn consolidate_if_needed(&mut self, tau: usize) -> Result<bool, String> {
        if !self.index.needs_consolidation() && self.index.ops_since_consolidation() < tau {
            return Ok(false);
        }
        if self.id_map.len() != self.index.num_vectors() {
            return Err("consolidation: id_map length differs from vector count".to_string());
        }
        let new_to_old = self
            .index
            .consolidate()
            .map_err(|e| format!("memory ANN consolidation: {e}"))?;
        if !new_to_old.is_empty() {
            self.id_map = new_to_old
                .into_iter()
                .map(|old| {
                    self.id_map.get(old as usize).copied().ok_or_else(|| {
                        format!("consolidation: old ordinal {old} is outside id_map")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        if self.reverse_map.is_some() {
            self.rebuild_reverse_map();
        }
        Ok(true)
    }

    /// Save this bridge to `dir` atomically: v2 Vamana segments (commit
    /// record is the gate), then the id-map sidecar bound to the blake3
    /// digest of that record. A crash between the two writes leaves a
    /// digest mismatch that `load` detects as a torn pair. Once both are
    /// committed, the retired delta HEAD and its chunks are removed; a failure
    /// there is logged rather than returned, because readers already ignore a
    /// HEAD whose watermark the new base covers.
    pub(crate) fn save_atomic(&self, dir: &std::path::Path) -> Result<[u8; 32], String> {
        let count = self.id_map.len();
        if count != self.index.num_vectors() {
            return Err(format!(
                "id_map length {count} != index.num_vectors() {}",
                self.index.num_vectors()
            ));
        }
        self.index
            .save_atomic(dir)
            .map_err(|e| format!("VamanaIndex::save_atomic: {e}"))?;
        let digest = segment_commit_digest(dir)
            .map_err(|e| format!("segment_commit_digest after save: {e}"))?
            .ok_or_else(|| {
                "save_atomic succeeded but metadata.bin is absent (torn commit)".to_string()
            })?;
        write_external_ids_sidecar(dir, &digest, &self.id_map).map_err(|e| e.to_string())?;
        if let Err(error) = delta::clear(dir) {
            tracing::warn!(%error, "memory delta cleanup failed after full checkpoint commit");
        }
        Ok(digest)
    }

    /// Load a bridge from a segment directory written by `save_atomic`. Any
    /// missing, torn, or cross-check-failing state returns `Err`; the caller
    /// treats that as a Cold signal. `namespace_set` starts empty
    /// (conservative — recall assumes non-visible namespaces may exist) until
    /// the caller populates it.
    pub(crate) fn load(dir: &std::path::Path) -> Result<Self, String> {
        read_commit_fingerprint(dir)
            .map_err(|e| format!("read_commit_fingerprint: {e}"))?
            .ok_or_else(|| {
                "no v2 commit fingerprint: segment dir is absent, v1, or has a torn commit"
                    .to_string()
            })?;
        let index = VamanaIndex::load(dir).map_err(|e| format!("VamanaIndex::load: {e}"))?;
        let (sidecar_digest, id_map) = read_external_ids_sidecar(dir)?;
        let commit_digest = segment_commit_digest(dir)
            .map_err(|e| format!("segment_commit_digest: {e}"))?
            .ok_or_else(|| "metadata.bin vanished between fingerprint and digest".to_string())?;
        if sidecar_digest != commit_digest {
            return Err(
                "external_ids.bin commit-digest mismatch: torn segment/sidecar pair".to_string(),
            );
        }
        if id_map.len() != index.num_vectors() {
            return Err(format!(
                "external_ids.bin count {} != index.num_vectors() {}",
                id_map.len(),
                index.num_vectors()
            ));
        }
        let base_applied_seq = index.last_applied_seq().unwrap_or(0);
        let base_ops = index.num_vectors();
        let mut bridge = Self {
            index,
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: None,
            id_map,
            reverse_map: None,
            #[cfg(test)]
            reverse_map_builds: 0,
            dirty_ops: 0,
            published_seq: base_applied_seq,
            last_checkpoint: std::time::Instant::now(),
            commit_digest: Some(commit_digest),
            base_commit_digest: Some(commit_digest),
            base_applied_seq,
            base_ops,
            delta_batches: Vec::new(),
            delta_raw_ops: 0,
            delta_chunks: 0,
            last_delta_nonce: None,
            namespace_set: HashSet::new(),
            generation: 0,
            epoch_baseline: 0,
            #[cfg(test)]
            drop_probe: None,
        };
        if let Some(overlay) = delta::read(
            dir,
            &commit_digest,
            base_applied_seq,
            bridge.index.dimensions(),
            base_ops,
        )? {
            for batch in &overlay.batches {
                bridge.apply_final_ops(batch.ops.clone(), batch.applied_seq)?;
            }
            bridge.published_seq = overlay.applied_seq;
            bridge.commit_digest = Some(overlay.identity);
            bridge.delta_raw_ops = overlay.raw_count;
            bridge.delta_chunks = overlay.batches.len();
            bridge.last_delta_nonce = Some(overlay.last_nonce);
        }
        Ok(bridge)
    }

    /// Populate `namespace_set` from an already-queried set of namespace strings.
    pub(crate) fn set_namespace_set(&mut self, ns_set: HashSet<String>) {
        self.namespace_set = ns_set;
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Replace non-alphanumeric chars with `_` to produce a valid table-name suffix.
pub(crate) fn sanitize_model_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Identity key for the global memory Vamana index for a model; hex-encoded to
/// name its v2 segment directory. Distinct from knowledge's
/// `{ns}::vamana::{model}` to prevent corpus identity collisions.
pub(crate) fn snapshot_key(_namespace: &str, model: &str) -> String {
    format!("global::memory_vamana::{model}")
}

/// Status returned by `ensure_ann_for_model` so callers can log/act on the
/// build outcome without parsing log lines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnEnsureStatus {
    AlreadyLoaded,
    LoadedSnapshot,
    Built {
        vectors: usize,
    },
    EmptyCorpus,
    DiscardedStaleBuild,
    /// Nothing on disk was adoptable and this process does not build corpus
    /// indexes. The caller serves its exact/lexical path for this request; the
    /// daemon builds and publishes, and the next attempt adopts that segment.
    DeclinedNotWarmHost,
}

// ── state operations ──────────────────────────────────────────────────────────

/// Search the already-loaded index for `key`. Returns `Ok(None)` on cache miss,
/// `Ok(Some(hits))` on success, `Err` on ANN search failure (caller falls back).
/// Test-only: production call sites need the watermark paired atomically with
/// the search (ADR-118) and use [`search_loaded_with_seq`] directly.
#[cfg(test)]
pub(crate) async fn search_loaded(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Result<Option<Vec<(Uuid, f32)>>, RuntimeError> {
    search_loaded_with_seq(ann, key, query, k)
        .await
        .map(|opt| opt.map(|(hits, _seq)| hits))
}

/// Same as [`search_loaded`], but also returns the searched bridge's
/// fresh-tail watermark (ADR-118), captured from the SAME read-lock guard as
/// the search itself — one lock acquisition, so a concurrent bridge swap
/// (a checkpoint installing a replacement) cannot pair these candidates with
/// a different bridge's watermark.
pub(crate) async fn search_loaded_with_seq(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Result<Option<(Vec<(Uuid, f32)>, u64)>, RuntimeError> {
    search_loaded_with_seq_route(ann, key, query, k, AnnScoreRoute::Memory)
        .await
        .map(|result| {
            result.map(|(hits, seq)| {
                (
                    hits.into_iter()
                        .map(|(id, score)| (id, score as f32))
                        .collect(),
                    seq,
                )
            })
        })
}

pub(crate) async fn search_loaded_with_seq_route(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    route: AnnScoreRoute,
) -> Result<Option<(Vec<(Uuid, f64)>, u64)>, RuntimeError> {
    let guard = ann.indexes.read().await;
    match guard.get(key) {
        None => Ok(None),
        Some(bridge) => {
            #[cfg(test)]
            ann.warm_route_count.fetch_add(1, Ordering::SeqCst);
            let hits = bridge.search_with_route(query, k, route)?;
            // Keep the exact leg over unpublished deltas: incremental graph
            // insertion alone does not guarantee immediate reachability.
            let seq = if bridge.dirty_ops > 0 {
                bridge.published_seq
            } else {
                bridge.index.last_applied_seq().unwrap_or(0)
            };
            Ok(Some((hits, seq)))
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

/// Evict only the serving bridge after durable registry protection is lost.
/// The current warm owner keeps its fire-once guard until normal RAII release;
/// dropping that guard here would allow duplicate rebuild tasks to overlap.
async fn evict_unprotected_index(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
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
    _token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> bool {
    // Request-time recall and mutation hooks share this entry point with
    // daemon warm. A snapshot may use an already-loaded bridge or the exact
    // sqlite-vec fallback, but it must never enqueue registration,
    // checkpoint, or compaction work against its read-only backend.
    if rt.is_read_only() || model.is_empty() {
        return false;
    }
    let key = AnnKey::from_token(model);

    // Capture the generation before the fast path so the caller observes prior writes.
    let target_generation = current_generation(ann, &key).await;

    // Presence is insufficient: the installed generation must cover the caller's floor.
    // An installed memory bridge may predate this consumer's registration.
    // Its graph is useful to recall, but note search cannot trust its tail
    // until a full checkpoint activates the new watermark.
    let note_search_pending = note_search_requires_full_rebuild(rt, ann, model).await;
    if !note_search_pending
        && installed_is_fresh(ann, &key, target_generation).await
        && !checkpoint_due(ann, &key).await
    {
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
    khive_runtime::track_named_background_task("memory_ann_rebuild", async move {
        if chained {
            tokio::time::sleep(rebuild_chain_debounce()).await;
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

/// Whether `start_rotation_watcher` has claimed its one-shot guard for `ann`.
#[cfg(test)]
pub(crate) fn rotation_watch_started_for_test(ann: &SharedAnn) -> bool {
    ann.rotation_watch_started.load(Ordering::Acquire)
}

/// Start one pack-lifetime watcher that releases mmap file generations after
/// a peer checkpoint rotates them. The task holds only a weak ANN reference
/// between ticks, so dropping the pack ends it even in a non-daemon stdio
/// process; daemon shutdown is an immediate second exit path.
pub(crate) fn start_rotation_watcher(rt: &KhiveRuntime, ann: &SharedAnn) {
    drop(start_rotation_watcher_with_shutdown(
        rt,
        ann,
        khive_runtime::daemon_shutdown_token(),
    ));
}

fn start_rotation_watcher_with_shutdown(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    shutdown: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let ann_root = rt.backend_ann_root()?;
    if ann
        .rotation_watch_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }

    let ann = Arc::downgrade(ann);
    let tick = move || {
        let ann = ann.upgrade();
        let ann_root = ann_root.clone();
        async move {
            let Some(ann) = ann else {
                return std::ops::ControlFlow::Break(());
            };
            refresh_rotated_segments_in_root(&ann_root, &ann).await;
            std::ops::ControlFlow::Continue(())
        }
    };
    Some(khive_runtime::spawn_named_tracked_task(
        "memory_ann_rotation_watch",
        khive_retrieval::ann::rotation_watch_loop(ROTATION_WATCH_INTERVAL, shutdown, tick),
    ))
}

/// Poll every installed mmap bridge once and replace any generation published
/// by another process. This performs no corpus scan on the unchanged path.
#[cfg(test)]
async fn refresh_rotated_segments_once(rt: &KhiveRuntime, ann: &SharedAnn) {
    let Some(ann_root) = rt.backend_ann_root() else {
        return;
    };
    refresh_rotated_segments_in_root(&ann_root, ann).await;
}

async fn refresh_rotated_segments_in_root(ann_root: &std::path::Path, ann: &SharedAnn) {
    let installed: Vec<(AnnKey, [u8; 32])> = ann
        .indexes
        .read()
        .await
        .iter()
        .filter_map(|(key, bridge)| bridge.commit_digest.map(|digest| (key.clone(), digest)))
        .collect();

    for (key, expected) in installed {
        let dir = ann_segment_dir_from_root(ann_root, &key.model);
        match delta::publication_digest(&dir) {
            Ok(Some(observed)) if observed != expected => {
                refresh_rotated_segment(ann, &key, expected, dir).await;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    model = %key.model,
                    "memory ANN rotation check failed; retaining the installed generation"
                );
            }
        }
    }
}

/// Reload one changed publication under the same local and cross-process
/// locks used by checkpoint writers. A committed-but-invalid replacement
/// evicts the predecessor: its segment files have already been unlinked, and
/// retaining it would recreate the disk-space leak this watcher prevents.
async fn refresh_rotated_segment(
    ann: &SharedAnn,
    key: &AnnKey,
    expected: [u8; 32],
    dir: std::path::PathBuf,
) {
    let local_lock = model_warm_lock(ann, key).await;
    let _local_guard = local_lock.lock().await;

    let incumbent = ann.indexes.read().await.get(key).map(|bridge| {
        (
            bridge.commit_digest,
            bridge.generation,
            bridge.epoch_baseline,
            bridge.index.last_applied_seq().unwrap_or(0),
            bridge.published_seq,
            bridge.dirty_ops > 0,
        )
    });
    let Some((
        Some(incumbent_digest),
        generation,
        epoch_baseline,
        incumbent_seq,
        published_seq,
        dirty,
    )) = incumbent
    else {
        return;
    };
    if incumbent_digest != expected {
        return;
    }

    let _publication_guard = match acquire_bridge_checkpoint_lock_async(dir.clone()).await {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                error = %error,
                model = %key.model,
                "memory ANN rotation reload could not acquire the publication lock"
            );
            return;
        }
    };

    let observed = match delta::publication_digest(&dir) {
        Ok(Some(digest)) if digest != incumbent_digest => digest,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(
                error = %error,
                model = %key.model,
                "memory ANN rotated commit identity could not be read"
            );
            return;
        }
    };

    let replacement = load_segment(ann, &dir).and_then(|bridge| {
        if bridge.commit_digest != Some(observed) {
            return Err("commit identity changed during locked rotation reload".to_string());
        }
        let replacement_seq = bridge.index.last_applied_seq().unwrap_or(0);
        let protected_seq = if dirty { published_seq } else { incumbent_seq };
        if replacement_seq < protected_seq {
            return Err(format!(
                "rotated segment watermark {replacement_seq} regressed below installed {incumbent_seq}"
            ));
        }
        Ok(bridge
            .with_generation(generation)
            .with_epoch_baseline(epoch_baseline))
    });

    match replacement {
        Ok(bridge) => {
            // Do not carry the incumbent's namespace_set onto the rotated
            // bridge: a peer's checkpoint can cover namespaces this process
            // never observed. Leave the conservative empty set `load` set,
            // so recall keeps over-fetching until it repopulates.
            let must_replay = dirty && bridge.index.last_applied_seq().unwrap_or(0) < incumbent_seq;
            if install_replacing(ann, key, bridge).await {
                if must_replay {
                    // Release the old files, but retain freshness pressure.
                    // Recall merges the retained tail above the peer checkpoint.
                    bump_generation(ann, key).await;
                }
                tracing::debug!(
                    model = %key.model,
                    "memory ANN adopted rotated mmap generation and released its predecessor"
                );
            }
        }
        Err(error) => {
            let mut indexes = ann.indexes.write().await;
            if indexes
                .get(key)
                .is_some_and(|bridge| bridge.commit_digest == Some(incumbent_digest))
            {
                indexes.remove(key);
            }
            tracing::warn!(
                error = %error,
                model = %key.model,
                "memory ANN rotated generation failed validation; released predecessor and will rebuild on demand"
            );
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
    let key = AnnKey::from_token(model);

    // Cross-process registry state is checked before the in-memory fast
    // path: a peer compactor can retire a pending row, after which an
    // already-loaded bridge is no longer protected and must not be served.
    let mut force_full_rebuild = match read_own_watermark(rt, model).await {
        Ok(Some(watermark)) if watermark >= 0 => false,
        Ok(Some(PENDING_WATERMARK)) => {
            evict_unprotected_index(ann, &key).await;
            true
        }
        Ok(Some(watermark)) => {
            evict_unprotected_index(ann, &key).await;
            tracing::warn!(
                model,
                watermark,
                "memory ANN registry is in a closed recovery state; refusing stale serve"
            );
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry for {model} is in unsupported recovery state {watermark}"
            )));
        }
        Ok(None) => {
            evict_unprotected_index(ann, &key).await;
            register_consumer(rt, model)
                .await
                .map_err(RuntimeError::Internal)?;
            true
        }
        Err(error) => {
            evict_unprotected_index(ann, &key).await;
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry read failed before serve: {error}"
            )));
        }
    };
    // A graph published before Slice 3 has no note_search protection. Its
    // old tail may already have been compacted under the memory row, so only
    // a new full scan may activate this second consumer. Keep the memory
    // bridge available while that work runs.
    force_full_rebuild |= note_search_requires_full_rebuild(rt, ann, model).await;

    // Read generation BEFORE any fast path or corpus snapshot to close the write race.
    let target_generation = current_generation(ann, &key).await;

    // Fast path: no lock needed if already warm AND fresh enough.
    if !force_full_rebuild
        && installed_is_fresh(ann, &key, target_generation).await
        && !checkpoint_due(ann, &key).await
    {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    let lock = model_warm_lock(ann, &key).await;
    let _single_flight_guard = lock.lock().await;

    // A concurrent caller may have activated the pending registration and
    // satisfied our generation while we waited. Re-read durable state before
    // accepting its bridge; local presence alone cannot prove protection.
    match read_own_watermark(rt, model).await {
        Ok(Some(watermark)) if watermark >= 0 => {
            force_full_rebuild = note_search_requires_full_rebuild(rt, ann, model).await;
            if !force_full_rebuild
                && installed_is_fresh(ann, &key, target_generation).await
                && !checkpoint_due(ann, &key).await
            {
                return Ok(AnnEnsureStatus::AlreadyLoaded);
            }
        }
        Ok(Some(PENDING_WATERMARK)) => force_full_rebuild = true,
        Ok(None) => {
            register_consumer(rt, model)
                .await
                .map_err(RuntimeError::Internal)?;
            force_full_rebuild = true;
        }
        Ok(Some(watermark)) => {
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry for {model} is in unsupported recovery state {watermark}"
            )));
        }
        Err(error) => {
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry revalidation failed: {error}"
            )));
        }
    }
    force_full_rebuild |= note_search_requires_full_rebuild(rt, ann, model).await;

    let phase_start = std::time::Instant::now();
    // Process CPU is cumulative, so phase attribution requires entry and exit snapshots.
    let cpu_start = khive_runtime::process_resource_usage();
    // RAII keeps `ann_warm` visible to health reporting on every exit path.
    let _phase_guard = khive_runtime::register_active_phase("ann_warm");
    // No corpus count here: the Hot adoption path performs zero corpus I/O
    // (ADR-079 Amendment 1), and an O(N) diagnostic COUNT on every warm would
    // defeat that. Vector counts are attributed at build completion instead.
    let corpus_size = None;
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

    let mut details = AnnWarmDetails::default();
    let result = ensure_ann_for_model_inner(
        rt,
        token,
        ann,
        model,
        target_generation,
        force_full_rebuild,
        &mut details,
    )
    .await;

    let wall_us = phase_start.elapsed().as_micros() as i64;
    let cpu_us = khive_runtime::cpu_delta_us(cpu_start, khive_runtime::process_resource_usage());
    emit_ann_warm_terminal_phase(rt, token, model, &result, wall_us, cpu_us, details).await;
    if result.is_ok() {
        checkpoint_timer::schedule_checkpoint(rt, ann, &key).await;
    }
    result
}

async fn emit_ann_warm_terminal_phase(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    result: &Result<AnnEnsureStatus, RuntimeError>,
    wall_us: i64,
    cpu_us: Option<i64>,
    mut details: AnnWarmDetails,
) {
    details.finish(result);
    match result {
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
                AnnWarmCompletedPayload {
                    phase: khive_storage::PhaseCompletedPayload {
                        work_class: "warm".into(),
                        phase: "ann_warm".into(),
                        wall_us,
                        cpu_us,
                    },
                    path: details.path,
                    ops_applied: details.ops_applied,
                },
            )
            .await;
        }
    }
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
    let result = crate::store_access::acquire_store("memory.ann.event_store", {
        let runtime = rt.clone();
        let token = token.clone();
        move || runtime.events(&token)
    })
    .await;
    let Ok(Ok(store)) = result else {
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
    mut force_full_rebuild: bool,
    details: &mut AnnWarmDetails,
) -> Result<AnnEnsureStatus, RuntimeError> {
    let ns = "global";
    let key = AnnKey::new(model);

    if !force_full_rebuild
        && installed_is_fresh(ann, &key, target_generation).await
        && !checkpoint_due(ann, &key).await
    {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    // Stamp the epoch observed before this attempt; only a later reindex invalidates it.
    let target_epoch = durable_epoch(rt).await;
    let epoch_changed = ann
        .indexes
        .read()
        .await
        .get(&key)
        .is_some_and(|bridge| bridge.epoch_baseline != target_epoch);
    force_full_rebuild |= epoch_changed;
    if !force_full_rebuild && ann.builds_corpus_indexes {
        match maintain_installed(
            rt,
            ann,
            &key,
            model,
            target_generation,
            target_epoch,
            details,
        )
        .await?
        {
            InstalledMaintenance::Complete => return Ok(AnnEnsureStatus::AlreadyLoaded),
            InstalledMaintenance::Rebuild => force_full_rebuild = true,
            InstalledMaintenance::Absent => {}
        }
    }

    // v2 segment classifier (ADR-079 Amendment 1, global-scope addendum): the
    // 8-rule first-match decision table over the persisted commit record, this
    // consumer's wildcard registry row, and one same-snapshot (live, tail)
    // read. Replaces the retired JSON-snapshot content-hash gate.
    if !force_full_rebuild {
        if let Some(seg_dir) = ann_segment_dir(rt, model) {
            match classify_and_adopt_segment(
                rt,
                ann,
                &key,
                model,
                &seg_dir,
                target_generation,
                target_epoch,
                details,
            )
            .await
            {
                SegmentOutcome::Installed(status) => return Ok(status),
                SegmentOutcome::Empty => {
                    tracing::debug!(namespace = %ns, model = %model, "memory ANN: zero live corpus");
                    return Ok(AnnEnsureStatus::EmptyCorpus);
                }
                SegmentOutcome::Cold => {}
            }
        }
    }

    if !ann.builds_corpus_indexes {
        tracing::info!(namespace = %ns, model = %model,
            "no adoptable memory ANN segment and this process does not build corpus \
             indexes; serving degraded and leaving the build to the daemon");
        return Ok(AnnEnsureStatus::DeclinedNotWarmHost);
    }

    // The fingerprint sandwich bounds scan races; generation ordering closes the
    // later persistence/install window and prevents an older build from winning.
    details.path = "full_build";
    let empty_corpus_fence = ann
        .indexes
        .read()
        .await
        .get(&key)
        .map(MaintenanceFence::capture);
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
            let installed = checkpoint_raise_compact_readopt(
                rt,
                ann,
                &key,
                model,
                bridge,
                CheckpointPublication {
                    generation: target_generation,
                    epoch: target_epoch,
                    authority: WatermarkAuthority::PendingOrActive,
                },
            )
            .await;
            if !installed {
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            tracing::debug!(namespace = %ns, model = %model, vectors = vector_count, "memory ANN index built");
            Ok(AnnEnsureStatus::Built {
                vectors: vector_count,
            })
        }
        Ok(None) => {
            // An empty rebuild has no replacement to retire an incumbent retained
            // after private replay failed. Evict only that unchanged incarnation.
            khive_storage::ensure_request_read_active("memory.ann.empty_corpus")?;
            if empty_corpus_fence.is_some()
                && fp_before
                    .as_ref()
                    .is_none_or(|fingerprint| fingerprint.vector_count != 0)
            {
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            if durable_epoch(rt).await != target_epoch {
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            let retired = {
                let mut indexes = ann.indexes.write().await;
                let unchanged = match &empty_corpus_fence {
                    Some(fence) => indexes
                        .get(&key)
                        .is_some_and(|bridge| fence.matches(bridge)),
                    None => !indexes.contains_key(&key),
                };
                if !unchanged {
                    return Ok(AnnEnsureStatus::DiscardedStaleBuild);
                }
                khive_storage::ensure_request_read_active("memory.ann.empty_corpus")?;
                indexes.remove(&key)
            };
            drop(retired);
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

/// Compute a fingerprint for all non-deleted memory note vectors for `model` (all namespaces).
async fn compute_memory_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let result = crate::store_access::acquire_store("memory.ann.fingerprint_store", {
        let runtime = rt.clone();
        let token = token.clone();
        let model = model.to_owned();
        move || runtime.vectors_for_model(&token, &model)
    })
    .await;
    let store = result.ok()?.ok()?;
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

/// Build a graph from live model vectors, capturing the corpus watermark in
/// the same statement as the scan (ADR-079 Amendment 1 linearization; see
/// `docs/api/ann-lifecycle.md`). Global-scope, join-filtered: every namespace,
/// live notes only.
async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
    let result = crate::store_access::acquire_store("memory.ann.vector_store", {
        let runtime = rt.clone();
        let token = token.clone();
        let model = model.to_owned();
        move || runtime.vectors_for_model(&token, &model)
    })
    .await?;
    let store = match result {
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
                "SELECT v.subject_id, v.embedding, n.namespace, \
                        MAX( \
                          (SELECT COALESCE(MAX(seq), 0) FROM ann_write_log \
                            WHERE embedding_model = ?1 \
                              AND kind = 'note' AND field = 'note.content'), \
                          (SELECT COALESCE(MAX(watermark), 0) \
                             FROM ann_consumer_watermark \
                            WHERE consumer = ?2 AND namespace = ?3 \
                              AND embedding_model = ?1 AND watermark >= 0) \
                        ) AS log_s \
                 FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL \
                 ORDER BY v.subject_id"
            ),
            params: vec![
                SqlValue::Text(model.to_owned()),
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(ANN_WILDCARD_NS.into()),
            ],
            label: Some("memory_ann_corpus_scan".into()),
        })
        .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let scan_watermark = rows
        .first()
        .and_then(|row| match row.get("log_s") {
            Some(SqlValue::Integer(n)) => u64::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0);

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
        // `as_chunks` is unstable on stable; keep `chunks_exact` until it lands.
        #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
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

    // SQ8 training and Vamana construction are CPU-bound; keep them off Tokio workers.
    let mut built =
        tokio::task::spawn_blocking(move || AnnBridge::build(flat, dims, id_map, namespace_set))
            .await
            .map_err(|e| {
                RuntimeError::Storage(StorageError::driver(
                    khive_storage::StorageCapability::Vectors,
                    "memory_ann_build",
                    e,
                ))
            })??;
    built.set_applied_seq(scan_watermark);
    Ok(Some(built))
}

// ── ADR-079 Amendment 1: segments, registry, classifier (global scope) ────────

/// Global-scope v2 segment directory for `model`: `<db-file>.ann/<hex(snapshot_key)>`
/// (see `docs/api/ann-lifecycle.md` for the segment-root layout). `None` for
/// in-memory backends.
fn ann_segment_dir(rt: &KhiveRuntime, model: &str) -> Option<std::path::PathBuf> {
    let ann_root = rt.backend_ann_root()?;
    Some(ann_segment_dir_from_root(&ann_root, model))
}

fn ann_segment_dir_from_root(ann_root: &std::path::Path, model: &str) -> std::path::PathBuf {
    let key = snapshot_key("global", model);
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    ann_root.join(hex)
}

async fn acquire_bridge_checkpoint_lock_async(
    dir: std::path::PathBuf,
) -> Result<std::fs::File, String> {
    khive_retrieval::ann::acquire_checkpoint_lock_async(dir, "memory ANN").await
}

/// Install `candidate`, replacing an equal-or-newer-generation incumbent but
/// never a strictly newer one (see `docs/api/ann-lifecycle.md` for why equal
/// generations still replace). Returns whether `candidate` was installed.
async fn install_replacing(ann: &SharedAnn, key: &AnnKey, candidate: AnnBridge) -> bool {
    match ann.indexes.write().await.entry(key.clone()) {
        std::collections::hash_map::Entry::Occupied(mut e)
            if e.get().generation <= candidate.generation =>
        {
            e.insert(candidate);
            true
        }
        std::collections::hash_map::Entry::Occupied(_) => {
            tracing::debug!(
                model = %key.model,
                "memory ANN replace skipped: cached entry is newer than this build"
            );
            false
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(candidate);
            true
        }
    }
}

/// Stable, scope-bearing consumer identity for the memory note index
/// (ADR-079 Amendment 1, global-scope addendum): pack name plus the corpus
/// predicate's field value.
const ANN_CONSUMER: &str = "memory-notes:note.content";
/// The same global note-content graph has a second ADR-118 consumer with its
/// own durable protection; it never borrows the memory consumer's row.
pub(crate) const NOTE_SEARCH_CONSUMER: &str = "note_search";

/// Registry namespace for a global-scope consumer (one row per model spanning
/// every namespace). `'*'` is not a valid `Namespace` value, so wildcard rows
/// cannot collide with per-namespace rows.
const ANN_WILDCARD_NS: &str = "*";

/// Durably register this consumer's wildcard row as pending (`-2`). MUST run
/// before the first full scan, persist, or serve: pending blocks compaction in
/// every namespace but, unlike an active checkpoint at `S = 0`, can be retired
/// with a visible warning if it never activates.
async fn register_consumer(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    register_consumer_identity(rt, model, ANN_CONSUMER).await
}

async fn register_consumer_identity(
    rt: &KhiveRuntime,
    model: &str,
    consumer: &str,
) -> Result<(), String> {
    let sql = rt.sql();
    if ann_segment_dir(rt, model).is_none() {
        // Pooled in-memory writers can't hold a manual transaction across
        // calls, so this stays a single statement; pathless runtimes have no
        // durable consumer to age-retire, so that's sufficient here.
        let mut writer = sql.writer().await.map_err(|e| e.to_string())?;
        writer
            .execute(SqlStatement {
                sql: "INSERT OR IGNORE INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES (?1, ?2, ?3, ?4)"
                    .into(),
                params: vec![
                    SqlValue::Text(consumer.into()),
                    SqlValue::Text(ANN_WILDCARD_NS.into()),
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(PENDING_WATERMARK),
                ],
                label: Some("memory_ann_register_pathless_consumer".into()),
            })
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    ann_registry::register_pending(sql.as_ref(), consumer, ANN_WILDCARD_NS, model)
        .await
        .map_err(|e| e.to_string())
}

/// Read this consumer's own wildcard registry watermark. `None` = no row
/// (decision rule 4: Cold after re-registering as pending).
async fn read_own_watermark(rt: &KhiveRuntime, model: &str) -> Result<Option<i64>, String> {
    read_consumer_watermark(rt, model, ANN_CONSUMER).await
}

pub(crate) async fn read_note_search_watermark(
    rt: &KhiveRuntime,
    model: &str,
) -> Result<Option<i64>, String> {
    read_consumer_watermark(rt, model, NOTE_SEARCH_CONSUMER).await
}

async fn note_search_requires_full_rebuild(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
) -> bool {
    if !note_search_consumer_enabled(ann) {
        return false;
    }
    match read_note_search_watermark(rt, model).await {
        Ok(Some(watermark)) if watermark >= 0 => false,
        Ok(Some(_)) => true,
        Ok(None) => {
            if let Err(error) = register_consumer_identity(rt, model, NOTE_SEARCH_CONSUMER).await {
                tracing::warn!(%error, model, "note-search ANN registration failed");
            }
            true
        }
        Err(error) => {
            tracing::warn!(%error, model, "note-search ANN registry read failed");
            false // Memory recall's existing consumer remains independently valid.
        }
    }
}

async fn advance_note_search_watermark(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
    s: u64,
    authority: WatermarkAuthority,
) {
    if !note_search_consumer_enabled(ann) {
        return;
    }
    // An incremental checkpoint cannot activate a pending consumer: only a
    // full-corpus checkpoint under PendingOrActive may do that.
    if authority == WatermarkAuthority::Active
        && !matches!(read_note_search_watermark(rt, model).await, Ok(Some(n)) if n >= 0)
    {
        return;
    }
    if let Err(error) =
        raise_consumer_watermark_with_authority(rt, model, NOTE_SEARCH_CONSUMER, s, authority).await
    {
        tracing::warn!(%error, model, "note-search ANN watermark remains unavailable");
    }
}

async fn read_consumer_watermark(
    rt: &KhiveRuntime,
    model: &str,
    consumer: &str,
) -> Result<Option<i64>, String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT watermark FROM ann_consumer_watermark \
                  WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                .into(),
            params: vec![
                SqlValue::Text(consumer.into()),
                SqlValue::Text(ANN_WILDCARD_NS.into()),
                SqlValue::Text(model.to_owned()),
            ],
            label: Some("memory_ann_read_own_watermark".into()),
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| match row.get("watermark") {
            Some(SqlValue::Integer(n)) => Some(*n),
            _ => None,
        }))
}

/// Conditionally raise this consumer's registered watermark after a durable
/// segment commit at `s`. Retirement or a newer publisher causes a fenced
/// failure rather than installing state whose tail is no longer protected.
async fn raise_watermark_with_authority(
    rt: &KhiveRuntime,
    model: &str,
    s: u64,
    authority: WatermarkAuthority,
) -> Result<(), String> {
    raise_consumer_watermark_with_authority(rt, model, ANN_CONSUMER, s, authority).await
}

async fn raise_consumer_watermark_with_authority(
    rt: &KhiveRuntime,
    model: &str,
    consumer: &str,
    s: u64,
    authority: WatermarkAuthority,
) -> Result<(), String> {
    let sql = rt.sql();
    let raised = if ann_segment_dir(rt, model).is_none() {
        let watermark = i64::try_from(s)
            .map_err(|_| format!("memory ANN watermark {s} exceeds SQLite INTEGER range"))?;
        let predicate = match authority {
            WatermarkAuthority::PendingOrActive => {
                "(watermark = -2 OR (watermark >= 0 AND watermark <= ?4))"
            }
            WatermarkAuthority::Active => "watermark >= 0 AND watermark <= ?4",
            WatermarkAuthority::Recovering => "watermark = -1",
        };
        let mut writer = sql.writer().await.map_err(|e| e.to_string())?;
        writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE ann_consumer_watermark SET watermark = ?4 \
                     WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3 \
                       AND {predicate}"
                ),
                params: vec![
                    SqlValue::Text(consumer.into()),
                    SqlValue::Text(ANN_WILDCARD_NS.into()),
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(watermark),
                ],
                label: Some("memory_ann_raise_pathless_watermark".into()),
            })
            .await
            .map_err(|e| e.to_string())?
            == 1
    } else {
        ann_registry::raise_watermark(sql.as_ref(), consumer, ANN_WILDCARD_NS, model, s, authority)
            .await
            .map_err(|e| e.to_string())?
    };
    if !raised {
        return Err(format!(
            "{consumer} ANN watermark publication fence rejected {authority:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
async fn raise_watermark(rt: &KhiveRuntime, model: &str, s: u64) -> Result<(), String> {
    raise_watermark_with_authority(rt, model, s, WatermarkAuthority::Active).await
}

/// Compact the write log for `model` across every namespace, each bounded by
/// its own wildcard-inclusive registry minimum (ADR-079 Amendment 1 §A step
/// 3). A namespace with no registered rows yields `seq <= NULL`, which
/// matches nothing.
async fn compact_log(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    let sql = rt.sql();
    if ann_segment_dir(rt, model).is_none() {
        // The ephemeral, single-process backend has no durable dormant
        // registrations to retire. Keep its historical single-statement
        // compaction shape so a background checkpoint cannot expose a manual
        // multi-statement transaction between pooled writer operations.
        let mut writer = sql.writer().await.map_err(|e| e.to_string())?;
        writer
            .execute(SqlStatement {
                sql: "DELETE FROM ann_write_log \
                      WHERE embedding_model = ?1 \
                        AND seq <= (SELECT MIN(watermark.watermark) \
                                    FROM ann_consumer_watermark watermark \
                                    WHERE (watermark.namespace = ann_write_log.namespace \
                                           OR watermark.namespace = '*') \
                                      AND watermark.embedding_model = ?1)"
                    .into(),
                params: vec![SqlValue::Text(model.to_owned())],
                label: Some("memory_ann_compact_pathless_log".into()),
            })
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    ann_registry::compact_write_log(sql.as_ref(), CompactionScope::Model, model)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) async fn compact_log_for_test(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    compact_log(rt, model).await
}

/// Live corpus count and tail count for this consumer's scope, captured in ONE
/// statement so both come from the same SQLite read snapshot. Live is the
/// join-filtered global corpus; the tail spans every namespace under the same
/// kind/field predicate as the corpus scan.
async fn scope_counts(rt: &KhiveRuntime, model: &str, s: u64) -> Result<(u64, u64), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT \
                   (SELECT COUNT(*) FROM {table_name} v \
                     JOIN notes n ON n.id = v.subject_id \
                     WHERE v.embedding_model = ?1 \
                       AND v.kind = 'note' AND v.field = 'note.content' \
                       AND n.deleted_at IS NULL) AS live, \
                   (SELECT COUNT(*) FROM ann_write_log \
                     WHERE embedding_model = ?1 \
                       AND kind = 'note' AND field = 'note.content' \
                       AND seq > ?2) AS tail"
            ),
            params: vec![
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
            ],
            label: Some("memory_ann_scope_counts".into()),
        })
        .await
        .map_err(|e| e.to_string())?;
    let row = rows
        .into_iter()
        .next()
        .ok_or("scope_counts returned no row")?;
    let get = |col: &str| match row.get(col) {
        Some(SqlValue::Integer(n)) => u64::try_from(*n).map_err(|_| format!("negative {col}")),
        other => Err(format!("scope_counts {col}: unexpected value {other:?}")),
    };
    Ok((get("live")?, get("tail")?))
}

/// Log-table-only probe: does any write-log row exist above `s`? Touches no
/// corpus table, so the Hot path (empty tail) adopts with zero corpus I/O
/// (ADR-079 Amendment 1 restart classifier, rule 6; see `docs/ann.md`).
async fn tail_exists(rt: &KhiveRuntime, model: &str, s: u64) -> Result<bool, String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT EXISTS(SELECT 1 FROM ann_write_log \
                    WHERE embedding_model = ?1 \
                      AND kind = 'note' AND field = 'note.content' \
                      AND seq > ?2) AS has_tail"
                .into(),
            params: vec![
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
            ],
            label: Some("memory_ann_tail_exists".into()),
        })
        .await
        .map_err(|e| e.to_string())?;
    match rows.first().and_then(|row| row.get("has_tail")) {
        Some(SqlValue::Integer(n)) => Ok(*n != 0),
        other => Err(format!("tail_exists: unexpected value {other:?}")),
    }
}

/// Fetch the scope's tail (rows above `s`, all namespaces, ordered), coalesce
/// to the final op per subject, and hydrate embeddings for final upserts. A
/// missing vector row is a log/corpus contradiction (`Err` → Cold); a present
/// row whose note fails the join predicate replays as a delete (see
/// `docs/ann.md` for the full replay-outcome table). One statement is the
/// snapshot boundary, including for pool-backed readers. Returns the ops and
/// the new watermark.
pub(crate) async fn fetch_final_tail(
    rt: &KhiveRuntime,
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> Result<(Vec<(Uuid, Option<Vec<f32>>)>, u64), String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    fetch_final_tail_on(reader.as_mut(), model, s, live_threshold).await
}

/// Same as [`fetch_final_tail`] but runs its single snapshot statement through
/// a caller-supplied reader instead of acquiring its own handle. Serving paths
/// also use this seam while coordinating the ADR-118 §1 registry guard.
async fn fetch_final_tail_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> Result<(Vec<(Uuid, Option<Vec<f32>>)>, u64), String> {
    // `live_threshold` (ADR-118 §3): caps the scan to ceil(threshold × live
    // corpus) newest rows in one statement snapshot; the outer ORDER BY
    // restores ascending order so coalescing stays last-write-wins.
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let (live_cte, order_limit) = match live_threshold {
        Some(_) => (
            format!(
                "live AS (\
                   SELECT COUNT(*) AS live_count FROM {table_name} v \
                   JOIN notes n ON n.id = v.subject_id \
                   WHERE v.embedding_model = ?1 \
                     AND v.kind = 'note' AND v.field = 'note.content' \
                     AND n.deleted_at IS NULL\
                 ), "
            ),
            "ORDER BY seq DESC \
             LIMIT (SELECT CAST(live_count * ?3 AS INTEGER) + \
                       CASE WHEN CAST(live_count * ?3 AS INTEGER) < live_count * ?3 \
                            THEN 1 ELSE 0 END FROM live)"
                .to_string(),
        ),
        None => (String::new(), "ORDER BY seq".to_string()),
    };
    let mut params = vec![
        SqlValue::Text(model.to_owned()),
        SqlValue::Integer(s as i64),
    ];
    if let Some(threshold) = live_threshold {
        params.push(SqlValue::Float(threshold));
    }
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "WITH {live_cte}selected AS (\
                   SELECT seq, subject_id, op FROM ann_write_log \
                   WHERE embedding_model = ?1 \
                     AND kind = 'note' AND field = 'note.content' AND seq > ?2 \
                   {order_limit}\
                 ) \
                 SELECT selected.seq, selected.subject_id, selected.op, \
                        vectors.embedding_model AS vector_model, \
                        vectors.kind AS vector_kind, \
                        vectors.field AS vector_field, vectors.embedding, \
                        live_note.id AS live_note_id \
                 FROM selected \
                 LEFT JOIN {table_name} AS vectors \
                   ON vectors.subject_id = selected.subject_id \
                 LEFT JOIN notes AS live_note \
                   ON live_note.id = selected.subject_id \
                  AND live_note.deleted_at IS NULL \
                 ORDER BY selected.seq"
            ),
            params,
            label: Some("memory_ann_fresh_tail_snapshot".into()),
        })
        .await
        .map_err(|e| e.to_string())?;

    parse_final_tail_rows(&rows, model, s)
}

/// Read registry protection, capped raw delta size, and final vector state in
/// one statement. The count is exact when it is within `max_delta`; above the
/// cap, `max_delta + 1` rows suffice to require a rebuild without materializing
/// the remaining suffix. Pathless SQLite readers share the writer connection,
/// so keeping an explicit read transaction across several async reads would
/// retain the only pooled connection while the task is suspended.
async fn fetch_protected_tail_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
    s: u64,
    max_delta: u64,
) -> Result<(Option<(Vec<(Uuid, Option<Vec<f32>>)>, u64, u64)>, u64), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "WITH tail AS MATERIALIZED (\
                   SELECT seq, subject_id, op FROM ann_write_log \
                   WHERE embedding_model = ?1 \
                     AND kind = 'note' AND field = 'note.content' AND seq > ?2 \
                   ORDER BY seq LIMIT ?5\
                 ), summary AS MATERIALIZED (\
                   SELECT COUNT(*) AS raw_count, \
                          (SELECT MIN(watermark) FROM ann_consumer_watermark \
                           WHERE (namespace = ?3 OR namespace = '*') \
                             AND embedding_model = ?1) AS min_watermark \
                   FROM tail\
                 ), selected AS MATERIALIZED (\
                   SELECT seq, subject_id, op FROM tail \
                   WHERE (SELECT raw_count FROM summary) <= ?4\
                 ) \
                 SELECT 0 AS is_summary, summary.raw_count, summary.min_watermark, \
                        NULL AS seq, NULL AS subject_id, NULL AS op, \
                        NULL AS vector_model, NULL AS vector_kind, NULL AS vector_field, \
                        NULL AS embedding, NULL AS live_note_id \
                 FROM summary \
                 UNION ALL \
                 SELECT 1 AS is_summary, NULL AS raw_count, NULL AS min_watermark, \
                        selected.seq, selected.subject_id, selected.op, \
                        vectors.embedding_model AS vector_model, \
                        vectors.kind AS vector_kind, vectors.field AS vector_field, \
                        vectors.embedding, live_note.id AS live_note_id \
                 FROM selected \
                 LEFT JOIN {table_name} AS vectors \
                   ON vectors.subject_id = selected.subject_id \
                 LEFT JOIN notes AS live_note \
                   ON live_note.id = selected.subject_id \
                  AND live_note.deleted_at IS NULL \
                 ORDER BY is_summary, seq"
            ),
            params: vec![
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
                SqlValue::Text(ANN_WILDCARD_NS.to_owned()),
                SqlValue::Integer(max_delta.min(i64::MAX as u64) as i64),
                SqlValue::Integer(max_delta.saturating_add(1).min(i64::MAX as u64) as i64),
            ],
            label: Some("memory_ann_incremental_protected_tail".into()),
        })
        .await
        .map_err(|e| e.to_string())?;

    let summary = rows
        .first()
        .filter(|row| matches!(row.get("is_summary"), Some(SqlValue::Integer(0))))
        .ok_or_else(|| "incremental tail summary is missing".to_owned())?;
    let raw_count = match summary.get("raw_count") {
        Some(SqlValue::Integer(n)) if *n >= 0 => *n as u64,
        _ => return Err("incremental tail count is invalid".into()),
    };
    if let Some(SqlValue::Integer(minimum)) = summary.get("min_watermark") {
        if u64::try_from(*minimum).is_ok_and(|minimum| minimum > s) {
            return Err("installed watermark is behind compacted history".into());
        }
    } else if !matches!(summary.get("min_watermark"), Some(SqlValue::Null)) {
        return Err("incremental tail registry minimum is invalid".into());
    }
    if raw_count > max_delta {
        return Ok((None, raw_count));
    }

    let tail_rows = &rows[1..];
    let (ops, end) = parse_final_tail_rows(tail_rows, model, s)?;
    Ok((Some((ops, end, raw_count)), raw_count))
}

/// Final op per subject, in sequence order, plus the last applied sequence.
type FinalTail = (Vec<(Uuid, Option<Vec<f32>>)>, u64);

fn parse_final_tail_rows(
    rows: &[khive_storage::types::SqlRow],
    model: &str,
    s: u64,
) -> Result<FinalTail, String> {
    let mut new_s = s;
    type RawVector = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
    );
    // Ordered iteration + insert-overwrite = final op per subject wins.
    let mut finals: Vec<(Uuid, bool, RawVector, bool)> = Vec::new();
    let mut index_of: HashMap<Uuid, usize> = HashMap::new();
    for row in rows {
        let seq = match row.get("seq") {
            Some(SqlValue::Integer(n)) => *n,
            _ => return Err("ann_write_log.seq: unexpected value".into()),
        };
        new_s = new_s.max(u64::try_from(seq).map_err(|_| "negative seq")?);
        let uuid = match row.get("subject_id") {
            Some(SqlValue::Text(t)) => {
                Uuid::parse_str(t).map_err(|e| format!("tail subject_id {t}: {e}"))?
            }
            _ => return Err("ann_write_log.subject_id: unexpected value".into()),
        };
        let is_delete = match row.get("op") {
            Some(SqlValue::Text(t)) => t == "delete",
            _ => return Err("ann_write_log.op: unexpected value".into()),
        };
        let raw_vector = (
            match row.get("vector_model") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("vector_kind") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("vector_field") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("embedding") {
                Some(SqlValue::Blob(value)) => Some(value.clone()),
                _ => None,
            },
        );
        let is_live = matches!(
            row.get("live_note_id"),
            Some(SqlValue::Text(id)) if Uuid::parse_str(id).ok() == Some(uuid)
        );
        match index_of.get(&uuid) {
            Some(&i) => finals[i] = (uuid, is_delete, raw_vector, is_live),
            None => {
                index_of.insert(uuid, finals.len());
                finals.push((uuid, is_delete, raw_vector, is_live));
            }
        }
    }

    let mut ops: Vec<(Uuid, Option<Vec<f32>>)> = Vec::with_capacity(finals.len());
    for (subject, is_delete, (vector_model, vector_kind, vector_field, embedding), is_live) in
        finals
    {
        if is_delete {
            ops.push((subject, None));
            continue;
        }
        // A vec row that is absent or outside this consumer's scope
        // contradicts the committed upsert. The LEFT JOIN keeps that absence
        // visible to this check instead of silently dropping the log row.
        if vector_model.is_none()
            && vector_kind.is_none()
            && vector_field.is_none()
            && embedding.is_none()
        {
            return Err(format!(
                "tail upsert {subject}: vector row absent (log/corpus contradiction)"
            ));
        }
        let in_scope = vector_model.as_deref() == Some(model)
            && vector_kind.as_deref() == Some("note")
            && vector_field.as_deref() == Some("note.content");
        if !in_scope {
            return Err(format!(
                "tail upsert {subject}: vector row out of consumer scope (log/corpus contradiction)"
            ));
        }
        let Some(bytes) = embedding else {
            return Err(format!("tail upsert {subject}: embedding is not a blob"));
        };
        // `as_chunks` is unstable on stable; keep `chunks_exact` until it lands.
        #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
        let vector: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if is_live {
            ops.push((subject, Some(vector)));
        } else {
            // A vector whose note is soft-deleted or gone is a valid
            // join-filtered absence, represented as a delete in the replay.
            ops.push((subject, None));
        }
    }
    Ok((ops, new_s))
}

// ── ADR-118: fresh-tail exact leg (read-your-writes recall visibility) ─────────

/// Read the currently-installed bridge's fresh-tail watermark for `key`: the
/// `ann_write_log` seq its corpus state reflects, or `None` if no bridge is
/// installed. Test-only: production call sites need this paired atomically
/// with the search that produced their candidates (ADR-118) and get it from
/// [`search_loaded_with_seq`] directly instead of a second lock acquisition.
#[cfg(test)]
pub(crate) async fn bridge_applied_seq(ann: &SharedAnn, key: &AnnKey) -> Option<u64> {
    let guard = ann.indexes.read().await;
    guard
        .get(key)
        .map(|b| b.index.last_applied_seq().unwrap_or(0))
}

/// The wildcard-inclusive registry minimum (ADR-118 §1 "Compaction
/// linearization"; see `docs/ann.md`): the same bound `compact_log` uses. If
/// this exceeds a bridge's watermark, completeness above it is unprovable.
async fn registry_min_watermark_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
) -> Result<Option<i64>, String> {
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT MIN(watermark) AS m FROM ann_consumer_watermark \
                  WHERE (namespace = ?1 OR namespace = '*') AND embedding_model = ?2"
                .into(),
            params: vec![
                SqlValue::Text(ANN_WILDCARD_NS.into()),
                SqlValue::Text(model.to_owned()),
            ],
            label: Some("memory_ann_registry_min".into()),
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().next().and_then(|row| match row.get("m") {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }))
}

/// Read the named consumer inside the same snapshot as the registry minimum
/// and fresh-tail rows. An outside-the-snapshot precheck cannot protect a
/// graph from concurrent consumer retirement and compaction.
async fn consumer_watermark_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
    consumer: &str,
) -> Result<Option<i64>, String> {
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT watermark FROM ann_consumer_watermark \
                  WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                .into(),
            params: vec![
                SqlValue::Text(consumer.into()),
                SqlValue::Text(ANN_WILDCARD_NS.into()),
                SqlValue::Text(model.to_owned()),
            ],
            label: Some("note_search_ann_consumer_snapshot".into()),
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| match row.get("watermark") {
            Some(SqlValue::Integer(value)) => Some(*value),
            _ => None,
        }))
}

/// Open an explicit read transaction so a reader keeps one connection across
/// calls (ADR-118 §1). Load-bearing corpus hydration still uses one
/// statement (see [`fetch_final_tail_on`]) since a pool-backed reader may
/// reacquire a connection per call. `BEGIN DEFERRED` is valid read-only.
async fn begin_read_snapshot(reader: &mut dyn khive_storage::SqlReader) -> Result<(), String> {
    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: Some("memory_ann_fresh_tail_snapshot_begin".into()),
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Close the snapshot opened by [`begin_read_snapshot`]. The transaction is
/// read-only (no DML issued inside it), so `COMMIT` and `ROLLBACK` are
/// equivalent — `COMMIT` is used for symmetry with a normal transaction end.
async fn end_read_snapshot(reader: &mut dyn khive_storage::SqlReader) {
    if let Err(e) = reader
        .query_all(SqlStatement {
            sql: "COMMIT".into(),
            params: vec![],
            label: Some("memory_ann_fresh_tail_snapshot_end".into()),
        })
        .await
    {
        tracing::warn!(error = %e, "fresh-tail: snapshot COMMIT failed (read-only transaction; the connection is dropped regardless, so no lock is leaked)");
    }
}

/// Exact cosine similarity between a raw query vector and a raw stored
/// embedding, using the same L2-normalization convention as
/// [`AnnBridge::search`]: both vectors normalized, dot product, clamped to a
/// non-negative floor.
pub(crate) fn exact_cosine(query: &[f32], embedding: &[f32]) -> f32 {
    exact_cosine_unclamped(query, embedding).max(0.0)
}

fn exact_cosine_unclamped(query: &[f32], embedding: &[f32]) -> f32 {
    let mut q = query.to_vec();
    l2_normalize(&mut q);
    let mut e = embedding.to_vec();
    l2_normalize(&mut e);
    q.iter().zip(e.iter()).map(|(a, b)| a * b).sum::<f32>()
}

pub(crate) fn merge_fresh_tail_for_route(
    best_raw: Vec<(Uuid, f64)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
    route: AnnScoreRoute,
) -> Result<Vec<(Uuid, f64)>, RuntimeError> {
    if ops.is_empty() {
        return Ok(best_raw);
    }
    let mut deletes: HashSet<Uuid> = HashSet::new();
    let mut upserts: HashMap<Uuid, f64> = HashMap::new();
    for (uuid, op) in ops {
        match op {
            None => {
                deletes.insert(uuid);
            }
            Some(embedding) => {
                upserts.insert(uuid, route.tail_score(query, &embedding)?);
            }
        }
    }
    let mut merged: Vec<(Uuid, f64)> = best_raw
        .into_iter()
        .filter(|(uuid, _)| !deletes.contains(uuid) && !upserts.contains_key(uuid))
        .collect();
    merged.extend(upserts);
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(merged)
}

/// Keep the cheap wait probe and candidate-producing snapshot on the same
/// original-log-or-published-watermark fence predicate.
fn session_fence_predicate(
    model_param: usize,
    seq_param: usize,
    namespace_param: usize,
    consumer_param: usize,
    wildcard_param: usize,
) -> String {
    format!(
        "EXISTS(SELECT 1 FROM ann_write_log \
                  WHERE seq = ?{seq_param} AND namespace = ?{namespace_param} \
                    AND embedding_model = ?{model_param} \
                    AND kind = 'note' AND field = 'note.content' \
                    AND op = 'upsert') \
         OR EXISTS(SELECT 1 FROM ann_consumer_watermark \
                    WHERE consumer = ?{consumer_param} AND namespace = ?{wildcard_param} \
                      AND embedding_model = ?{model_param} \
                      AND watermark >= ?{seq_param} AND watermark >= 0)"
    )
}

/// Poll only the fence while a session recall waits. This is a scheduling hint,
/// not the candidate proof: `session_exact_candidates` repeats the same
/// predicate inside its candidate-producing SQLite snapshot.
pub(crate) async fn session_unmet_fences(
    rt: &KhiveRuntime,
    fence: &crate::visibility::VisibilityFence,
) -> Vec<String> {
    let mut reader = match rt.sql().reader().await {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!(%error, "session fence probe could not open a reader");
            return fence
                .fences
                .iter()
                .map(|entry| entry.model.clone())
                .collect();
        }
    };
    let sql = format!(
        "SELECT ({}) AS has_fence",
        session_fence_predicate(1, 2, 3, 4, 5)
    );
    let mut unmet = Vec::new();
    for entry in &fence.fences {
        let Ok(seq) = i64::try_from(entry.ann_write_log_seq) else {
            unmet.push(entry.model.clone());
            continue;
        };
        #[cfg(test)]
        let _ = SESSION_FENCE_PROBE_COUNT.try_with(|count| count.set(count.get() + 1));
        let result = reader
            .query_scalar(SqlStatement {
                sql: sql.clone(),
                params: vec![
                    SqlValue::Text(entry.model.clone()),
                    SqlValue::Integer(seq),
                    SqlValue::Text(fence.namespace.clone()),
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(ANN_WILDCARD_NS.into()),
                ],
                label: Some("memory_session_fence_probe".into()),
            })
            .await;
        if !matches!(&result, Ok(Some(SqlValue::Integer(1)))) {
            if let Err(error) = result {
                tracing::warn!(model = %entry.model, %error, "session fence probe failed");
            }
            unmet.push(entry.model.clone());
        }
    }
    unmet
}

/// Return exact KNN candidates only when the same SQL statement also proves
/// the write fence. An original log row proves the un-compacted tail; this
/// consumer's active wildcard watermark proves a published segment covered a
/// compacted row. The exact scan and both proof alternatives share one SQLite
/// snapshot, so a separate preflight clock cannot certify later candidates.
/// Per-namespace KNN queries are materialized before their union is ranked,
/// so a global top-k cannot hide candidates in another visible namespace.
pub(crate) async fn session_exact_candidates(
    rt: &KhiveRuntime,
    model: &str,
    query: &[f32],
    visible_namespaces: &[String],
    receipt_namespace: &str,
    required_seq: u64,
    k: usize,
) -> Result<Option<Vec<(Uuid, f32)>>, String> {
    let Ok(required_seq) = i64::try_from(required_seq) else {
        // SQLite rowids cannot reach this otherwise well-formed future fence.
        return Ok(None);
    };
    if query.is_empty() || query.iter().any(|component| !component.is_finite()) {
        return Err("session exact query vector is empty or non-finite".into());
    }

    let table_name = format!("vec_{}", sanitize_model_key(model));
    let query_blob = query.iter().flat_map(|value| value.to_le_bytes()).collect();
    let mut params = vec![
        SqlValue::Blob(query_blob),
        SqlValue::Integer(i64::try_from(k).unwrap_or(i64::MAX)),
        SqlValue::Text(model.to_owned()),
        SqlValue::Integer(required_seq),
        SqlValue::Text(receipt_namespace.to_owned()),
        SqlValue::Text(ANN_CONSUMER.into()),
        SqlValue::Text(ANN_WILDCARD_NS.into()),
    ];
    let mut seen = HashSet::new();
    let mut knn_ctes = Vec::new();
    let mut union_arms = Vec::new();
    let scopes = if k == 0 { &[][..] } else { visible_namespaces };
    for namespace in scopes {
        if !seen.insert(namespace.as_str()) {
            continue;
        }
        let index = knn_ctes.len();
        let parameter = params.len() + 1;
        params.push(SqlValue::Text(namespace.clone()));
        knn_ctes.push(format!(
            "session_knn_{index} AS MATERIALIZED ( \
               SELECT v.subject_id, v.namespace AS vector_namespace, distance \
                 FROM {table_name} v \
                WHERE embedding MATCH ?1 AND namespace = ?{parameter} \
                  AND embedding_model = ?3 AND kind = 'note' AND field = 'note.content' \
                ORDER BY distance LIMIT ?2)"
        ));
        union_arms.push(format!(
            "SELECT subject_id, vector_namespace, distance FROM session_knn_{index}"
        ));
    }
    let union = if union_arms.is_empty() {
        "SELECT NULL AS subject_id, NULL AS vector_namespace, NULL AS distance WHERE 0".into()
    } else {
        union_arms.join(" UNION ALL ")
    };
    let knn_ctes = if knn_ctes.is_empty() {
        String::new()
    } else {
        format!("{}, ", knn_ctes.join(", "))
    };
    let proof = session_fence_predicate(3, 4, 5, 6, 7);
    let sql = format!(
        "WITH session_proof AS MATERIALIZED ( \
           SELECT ({proof}) AS has_fence), \
         {knn_ctes}session_union AS MATERIALIZED ({union}), \
         session_ranked AS MATERIALIZED ( \
           SELECT c.subject_id, c.distance FROM session_union c \
           JOIN notes n ON n.id = c.subject_id \
             AND n.namespace = c.vector_namespace AND n.deleted_at IS NULL \
           ORDER BY c.distance, c.subject_id LIMIT ?2) \
         SELECT p.has_fence, r.subject_id, r.distance \
           FROM session_proof p LEFT JOIN session_ranked r ON 1 = 1 \
          ORDER BY r.distance, r.subject_id"
    );
    let mut reader = rt.sql().reader().await.map_err(|error| error.to_string())?;
    #[cfg(test)]
    let _ = SESSION_EXACT_STATEMENT_COUNT.try_with(|count| count.set(count.get() + 1));
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params,
            label: Some("memory_session_exact_snapshot".into()),
        })
        .await
        .map_err(|error| error.to_string())?;
    let first = rows
        .first()
        .ok_or("session exact statement returned no proof row")?;
    match first.get("has_fence") {
        Some(SqlValue::Integer(0)) => return Ok(None),
        Some(SqlValue::Integer(1)) => {}
        other => {
            return Err(format!(
                "session exact proof has unexpected value {other:?}"
            ))
        }
    }

    let mut candidates = Vec::with_capacity(rows.len().min(k));
    for row in rows {
        let (id, distance) = match (row.get("subject_id"), row.get("distance")) {
            (Some(SqlValue::Null), Some(SqlValue::Null)) => continue,
            (Some(SqlValue::Text(id)), Some(SqlValue::Float(distance))) => (id, *distance),
            other => {
                return Err(format!(
                    "session exact candidate has unexpected value {other:?}"
                ))
            }
        };
        let id = Uuid::parse_str(id).map_err(|error| format!("session exact id: {error}"))?;
        // Mirror khive-db's sqlite_cosine_score boundary tolerance, then use
        // the shared deterministic distance conversion (vectors.rs:170-196).
        const BOUNDARY_EPSILON: f64 = 8.0 * f32::EPSILON as f64;
        if !distance.is_finite()
            || !(-BOUNDARY_EPSILON..=2.0 + BOUNDARY_EPSILON).contains(&distance)
        {
            return Err(format!(
                "session exact cosine distance out of range: {distance}"
            ));
        }
        let score = khive_score::try_score_from_distance(
            distance.clamp(0.0, 2.0) as f32,
            khive_types::DistanceMetric::Cosine,
        )
        .map_err(|error| error.to_string())?
        .to_f64() as f32;
        candidates.push((id, score));
    }
    Ok(Some(candidates))
}

/// Merge a fresh-tail's coalesced final ops into an ANN candidate list
/// (ADR-118 §2): deduplicated by `subject_id` with the tail winning, then
/// re-sorted by score. A `None` op (delete) drops the subject even if it was
/// in `best_raw` — the tail is authoritative for every subject it names.
pub(crate) fn merge_fresh_tail(
    best_raw: Vec<(Uuid, f32)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
) -> Vec<(Uuid, f32)> {
    if ops.is_empty() {
        return best_raw;
    }
    let mut deletes: HashSet<Uuid> = HashSet::new();
    let mut upserts: HashMap<Uuid, f32> = HashMap::new();
    for (uuid, op) in ops {
        match op {
            None => {
                deletes.insert(uuid);
            }
            Some(embedding) => {
                upserts.insert(uuid, exact_cosine(query, &embedding));
            }
        }
    }
    let mut merged: Vec<(Uuid, f32)> = best_raw
        .into_iter()
        .filter(|(u, _)| !deletes.contains(u) && !upserts.contains_key(u))
        .collect();
    merged.extend(upserts);
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged
}

/// Fold a [`FreshTailOutcome`] into the candidates a recall handler serves
/// plus the degradation disclosure it owes the caller. Every recall path
/// goes through this single mapping so no exceptional class is silently
/// treated as healthy; see `docs/ann.md` for the per-variant disclosure rule.
pub(crate) fn outcome_into_candidates(
    outcome: FreshTailOutcome,
    prior: Vec<(Uuid, f32)>,
    query: &[f32],
) -> (Vec<(Uuid, f32)>, Option<String>) {
    match outcome {
        FreshTailOutcome::Ops(ops) => (merge_fresh_tail(prior, query, ops), None),
        FreshTailOutcome::Replace(candidates, reason) => (
            candidates
                .into_iter()
                .map(|(id, score)| (id, score as f32))
                .collect(),
            reason.map(str::to_string),
        ),
        FreshTailOutcome::Skipped(reason) => (prior, Some(reason.to_string())),
    }
}

/// Cap on the error text a [`SkipReason`] carries beside its label, counted
/// in characters and inclusive of [`SKIP_DETAIL_TRUNCATION_MARKER`]. A
/// degraded recall stamps the rendered reason onto every hit it serves, so an
/// error that quotes a whole statement must not be able to bloat the response.
const SKIP_DETAIL_MAX_CHARS: usize = 200;

/// Written in place of the characters a cut removed, so a reader can tell a
/// bounded rendering from a complete one.
const SKIP_DETAIL_TRUNCATION_MARKER: &str = "...";

/// Bound `detail` to [`SKIP_DETAIL_MAX_CHARS`] characters, marking a cut with
/// [`SKIP_DETAIL_TRUNCATION_MARKER`]. The bound is character-wise, not
/// byte-wise, so a multi-byte error message cannot be split mid-character.
fn bound_skip_detail(detail: &str) -> String {
    if detail.char_indices().nth(SKIP_DETAIL_MAX_CHARS).is_none() {
        return detail.to_owned();
    }
    let keep = SKIP_DETAIL_MAX_CHARS - SKIP_DETAIL_TRUNCATION_MARKER.chars().count();
    let cut = match detail.char_indices().nth(keep) {
        Some((byte_index, _)) => byte_index,
        None => detail.len(),
    };
    let mut bounded = String::with_capacity(cut + SKIP_DETAIL_TRUNCATION_MARKER.len());
    bounded.push_str(&detail[..cut]);
    bounded.push_str(SKIP_DETAIL_TRUNCATION_MARKER);
    bounded
}

/// Why the fresh-tail leg sat out a query: the failure-site label, plus the
/// error that caused the skip whenever the site was holding one.
///
/// One label covers causes that differ in what the caller should do next — a
/// reader pool exhausted for a moment clears on the next query, a database
/// file the process cannot open does not, and both arrive as "reader open
/// failed" — so the error travels out with the label instead of stopping at a
/// log line the caller cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkipReason {
    label: &'static str,
    detail: Option<String>,
}

impl SkipReason {
    /// A skip whose site holds no error: the bare label, unchanged.
    fn bare(label: &'static str) -> Self {
        Self {
            label,
            detail: None,
        }
    }

    /// A skip whose site holds the error that caused it. The error's own
    /// message is bounded by [`bound_skip_detail`] before it is carried.
    fn with_error(label: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            label,
            detail: Some(bound_skip_detail(&error.to_string())),
        }
    }

    /// The failure-site label on its own, without any error text.
    #[cfg(test)]
    fn label(&self) -> &'static str {
        self.label
    }

    /// The bounded error text, or `None` when the site held no error.
    #[cfg(test)]
    fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}: {detail}", self.label),
            None => f.write_str(self.label),
        }
    }
}

/// Outcome of [`fresh_tail_leg`]. Full semantics and the disclosure contract
/// for each variant: `docs/ann.md`.
pub(crate) enum FreshTailOutcome {
    /// Coalesced final tail ops, valid against the caller's existing
    /// candidates (`best_raw` unchanged) — merge via [`merge_fresh_tail`].
    /// The common case.
    Ops(Vec<(Uuid, Option<Vec<f32>>)>),
    /// A compaction mismatch forced re-resolution: these candidates REPLACE
    /// `best_raw` outright (never merge them with the stale set). `Some(reason)`
    /// discloses when read-your-writes visibility was lost after
    /// re-resolution; `None` means the full pair was assembled.
    Replace(Vec<(Uuid, f64)>, Option<&'static str>),
    /// The leg sat out this query entirely; the caller's candidates are
    /// unaffected. The payload renders to a non-empty failure-site
    /// diagnostic: the label, followed by the error that caused the skip
    /// whenever the site was holding one. Callers may depend on its
    /// presence, not on its exact wording.
    Skipped(SkipReason),
}

fn replace_with_merged_tail(
    candidates: Vec<(Uuid, f64)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
    route: AnnScoreRoute,
) -> FreshTailOutcome {
    if matches!(route, AnnScoreRoute::Memory) {
        let candidates = candidates
            .into_iter()
            .map(|(id, score)| (id, score as f32))
            .collect();
        return FreshTailOutcome::Replace(
            merge_fresh_tail(candidates, query, ops)
                .into_iter()
                .map(|(id, score)| (id, f64::from(score)))
                .collect(),
            None,
        );
    }
    match merge_fresh_tail_for_route(candidates, query, ops, route) {
        Ok(merged) => FreshTailOutcome::Replace(merged, None),
        Err(error) => FreshTailOutcome::Skipped(SkipReason::with_error(
            "fresh-tail: canonical score conversion failed",
            error,
        )),
    }
}

/// The ADR-118 fresh-tail exact leg, giving read-your-writes visibility.
/// `s = Some(watermark)` is the primary tier: merges every committed write
/// above the serving bridge's watermark. `s = None` is the no-index tier
/// (§3): caps the scan at a corpus-relative newest suffix instead of the
/// full scope. `query`/`k` serve only the primary tier's mismatch
/// re-resolution path. See `docs/ann.md` for both tiers in full.
pub(crate) async fn fresh_tail_leg(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    query: &[f32],
    k: usize,
    s: Option<u64>,
) -> FreshTailOutcome {
    // Registration precondition (ADR-118 §1): a serving bridge is trusted only
    // while its consumer is active (`S >= 0`). Pending/recovery/absence means a
    // peer may have retired the old protection; drop the already-captured ANN
    // candidates so even a disabled exact leg cannot leak stale state.
    let mut registry_state = read_own_watermark(rt, model).await;
    if s.is_some()
        && ann_segment_dir(rt, model).is_none()
        && matches!(registry_state, Ok(Some(PENDING_WATERMARK)))
    {
        // Wait out the pathless install-before-activation window (see
        // `docs/ann.md`) and revalidate, or the closed-state guard below
        // could evict a just-built, about-to-activate bridge.
        let lock = model_warm_lock(ann, key).await;
        #[cfg(test)]
        ann.pathless_pending_publication_wait.notify_one();
        let _publication_guard = lock.lock().await;
        registry_state = read_own_watermark(rt, model).await;
    }
    match registry_state {
        Ok(Some(watermark)) if watermark >= 0 => {}
        Ok(Some(watermark)) => {
            evict_unprotected_index(ann, key).await;
            tracing::warn!(
                model,
                watermark,
                "fresh-tail: closed consumer registration; dropping stale ANN candidates"
            );
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: closed consumer registration; dropped unprotected candidates"),
            );
        }
        Ok(None) => {
            evict_unprotected_index(ann, key).await;
            if let Err(e) = register_consumer(rt, model).await {
                tracing::warn!(error = %e, model, "fresh-tail: consumer re-registration failed");
            }
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: consumer registration absent; dropped unprotected candidates and re-registered"),
            );
        }
        Err(e) => {
            evict_unprotected_index(ann, key).await;
            tracing::warn!(error = %e, model, "fresh-tail: registry read failed; dropping stale ANN candidates");
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: registry read failed; dropped unprotected candidates"),
            );
        }
    }

    if !rt.ann_fresh_tail_enabled() {
        return FreshTailOutcome::Skipped(SkipReason::bare(
            "fresh-tail leg disabled by runtime policy (KHIVE_ANN_FRESH_TAIL is sampled at construction)",
        ));
    }

    match s {
        Some(s) => {
            fresh_tail_serving(
                rt,
                ann,
                key,
                model,
                FreshTailSearch::new(query, k, AnnScoreRoute::Memory),
                s,
                None,
            )
            .await
        }
        None => fresh_tail_capped(rt, model).await,
    }
}

/// Tier 1: a serving bridge exists at watermark `s`. The registry-minimum
/// guard and tail statement share one read transaction (ADR-118 §1
/// "Compaction linearization"; see `docs/ann.md`).
pub(crate) async fn fresh_tail_serving(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    search: FreshTailSearch<'_>,
    s: u64,
    consumer: Option<&str>,
) -> FreshTailOutcome {
    let mut reader = match rt.sql().reader().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: reader open failed; skipping exact leg");
            return FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: reader open failed",
                e,
            ));
        }
    };
    if let Err(e) = begin_read_snapshot(reader.as_mut()).await {
        tracing::warn!(error = %e, model, "fresh-tail: snapshot begin failed; skipping exact leg");
        return FreshTailOutcome::Skipped(SkipReason::with_error(
            "fresh-tail: snapshot begin failed",
            e,
        ));
    }
    if let Some(consumer) = consumer {
        let own = consumer_watermark_on(reader.as_mut(), model, consumer).await;
        if !matches!(own, Ok(Some(watermark)) if watermark >= 0) {
            end_read_snapshot(reader.as_mut()).await;
            return FreshTailOutcome::Skipped(SkipReason::bare(
                "note-search ANN consumer is not active in tail snapshot",
            ));
        }
    }

    let registry_min = match registry_min_watermark_on(reader.as_mut(), model).await {
        Ok(v) => v,
        Err(e) => {
            end_read_snapshot(reader.as_mut()).await;
            tracing::warn!(error = %e, model, "fresh-tail: registry-min read failed; skipping exact leg");
            return FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: registry-min read failed",
                e,
            ));
        }
    };

    // Negative (closed-state) minima must never be lossily cast to a huge
    // u64 floor — they cannot indicate that compaction outran `s`.
    if let Some(m) = registry_min.and_then(|value| u64::try_from(value).ok()) {
        if m > s {
            if ann_segment_dir(rt, model).is_none() {
                // A checkpoint may have raised the registry floor but not yet
                // marked its dirty bridge published. Never wait for that
                // checkpoint's model lock while pinning the pathless SQL
                // connection it needs to finish.
                end_read_snapshot(reader.as_mut()).await;
                drop(reader);
                return fresh_tail_pathless_reresolve(rt, ann, key, model, search, consumer).await;
            }

            // Mismatch (ADR-118 §1): the log may no longer retain every row
            // above `s`. A filesystem commit-record read (no DB access)
            // decides whether re-resolution is possible before this
            // snapshot's floor fallback is needed.
            let persisted = match ann_segment_dir(rt, model) {
                None => Ok(None),
                Some(dir) => match read_commit_info(&dir) {
                    Err(e) => Err(e.to_string()),
                    Ok(info) => match info.and_then(|info| info.last_applied_seq) {
                        None => Ok(None),
                        Some(base_seq) => {
                            effective_persisted_state(&dir, base_seq).map(|(seq, _)| Some(seq))
                        }
                    },
                },
            };
            let resolved_new_s = match persisted {
                Ok(seq) => seq.filter(|new_s| *new_s >= m),
                Err(e) => {
                    // An unreadable commit record or delta HEAD does not show
                    // that no newer segment exists. Compaction through `m`
                    // means a published state covered `m`, so a write it
                    // removed is in neither the stale candidates nor the log
                    // above `m`: no floored tail can complete them.
                    end_read_snapshot(reader.as_mut()).await;
                    tracing::warn!(error = %e, model, "fresh-tail: persisted segment state read failed; dropping stale ANN candidates");
                    bump_generation(ann, key).await;
                    return FreshTailOutcome::Replace(
                        Vec::new(),
                        Some("fresh-tail: persisted segment state read failed; dropped stale candidates"),
                    );
                }
            };
            return match resolved_new_s {
                Some(new_s) => {
                    end_read_snapshot(reader.as_mut()).await;
                    drop(reader);
                    fresh_tail_reresolve(rt, ann, key, model, search, new_s, consumer).await
                }
                None => {
                    // Missing publication metadata or a base below `m` cannot
                    // account for the compacted interval `(s, m]`. A tail
                    // above `m` cannot make the captured candidates complete.
                    end_read_snapshot(reader.as_mut()).await;
                    bump_generation(ann, key).await;
                    FreshTailOutcome::Replace(
                        Vec::new(),
                        Some("fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates"),
                    )
                }
            };
        }
    }

    let outcome = fetch_final_tail_on(reader.as_mut(), model, s, None).await;
    end_read_snapshot(reader.as_mut()).await;
    match outcome {
        Ok((ops, _new_s)) => FreshTailOutcome::Ops(ops),
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: tail fetch failed; skipping exact leg");
            FreshTailOutcome::Skipped(SkipReason::with_error("fresh-tail: tail fetch failed", e))
        }
    }
}

/// Resolve a pathless checkpoint mismatch without holding a SQL read snapshot
/// while waiting for the checkpoint's per-model lock. Once that checkpoint
/// finishes, the bridge search and a new registry/tail snapshot form one
/// coherent pair; this checkpoint's compaction cannot race the model lock.
async fn fresh_tail_pathless_reresolve(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    search: FreshTailSearch<'_>,
    consumer: Option<&str>,
) -> FreshTailOutcome {
    let FreshTailSearch { query, k, route } = search;
    let lock = model_warm_lock(ann, key).await;
    #[cfg(test)]
    ann.pathless_reresolve_wait_notify.notify_one();
    let _publication_guard = lock.lock().await;
    let (candidates, resolved_s) = match search_loaded_with_seq_route(ann, key, query, k, route)
        .await
    {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            bump_generation(ann, key).await;
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless mismatch has no installed bridge; dropped stale candidates"),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolution search failed; dropping stale ANN candidates");
            bump_generation(ann, key).await;
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless re-resolution search failed; dropped stale candidates"),
            );
        }
    };

    let mut reader = match rt.sql().reader().await {
        Ok(reader) => reader,
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved reader open failed");
            return FreshTailOutcome::Replace(
                candidates,
                Some("fresh-tail: pathless re-resolved reader open failed; served re-resolved candidates without fresh-tail merge"),
            );
        }
    };
    if let Err(e) = begin_read_snapshot(reader.as_mut()).await {
        tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved snapshot begin failed");
        return FreshTailOutcome::Replace(
            candidates,
            Some("fresh-tail: pathless re-resolved snapshot begin failed; served re-resolved candidates without fresh-tail merge"),
        );
    }
    if let Some(consumer) = consumer {
        let own = consumer_watermark_on(reader.as_mut(), model, consumer).await;
        if !matches!(own, Ok(Some(watermark)) if watermark >= 0) {
            end_read_snapshot(reader.as_mut()).await;
            return FreshTailOutcome::Skipped(SkipReason::bare(
                "note-search ANN consumer is not active after pathless re-resolution",
            ));
        }
    }
    let floor = registry_min_watermark_on(reader.as_mut(), model).await;
    let outcome = match floor {
        Ok(floor)
            if floor
                .and_then(|value| u64::try_from(value).ok())
                .is_none_or(|floor| floor <= resolved_s) =>
        {
            match fetch_final_tail_on(reader.as_mut(), model, resolved_s, None).await {
                Ok((ops, _)) => replace_with_merged_tail(candidates, query, ops, route),
                Err(e) => {
                    tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved tail fetch failed; serving re-resolved candidates");
                    FreshTailOutcome::Replace(
                        candidates,
                        Some("fresh-tail: pathless re-resolved tail fetch failed; served re-resolved candidates without fresh-tail merge"),
                    )
                }
            }
        }
        Ok(floor) => {
            tracing::warn!(
                ?floor,
                resolved_s,
                model,
                "fresh-tail: pathless re-resolved bridge is below the registry floor"
            );
            bump_generation(ann, key).await;
            FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless mismatch has no bridge at the registry floor; dropped stale candidates"),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: pathless re-resolved registry read failed");
            FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: pathless re-resolved registry read failed; dropped stale candidates"),
            )
        }
    };
    end_read_snapshot(reader.as_mut()).await;
    outcome
}

/// Bound on the re-resolution convergence loop below; three back-to-back peer
/// checkpoints inside one query's read window would be pathological. See
/// `docs/ann.md` for the convergence argument.
const FRESH_TAIL_RERESOLVE_MAX_ROUNDS: u32 = 3;

/// Real mismatch re-resolution (ADR-118 §1): load the currently published
/// segment, search it, and merge in its own tail above its own watermark — a
/// self-consistent pair that never borrows a newer watermark while serving
/// stale-bridge candidates. Reloads on a further mismatch instead of
/// immediately flooring, up to [`FRESH_TAIL_RERESOLVE_MAX_ROUNDS`]; see
/// `docs/ann.md` for why that reload converges and why flooring immediately
/// would silently drop committed writes.
async fn fresh_tail_reresolve(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    search: FreshTailSearch<'_>,
    new_s: u64,
    consumer: Option<&str>,
) -> FreshTailOutcome {
    let FreshTailSearch { query, k, route } = search;
    let mut expected_s = new_s;
    for round in 1..=FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
        // Every failure below that cannot produce re-resolved candidates drops
        // the stale ones: the caller chose re-resolution because compaction
        // passed their watermark, so a write it removed is in neither the
        // stale candidates nor the log above the registry minimum.
        let Some(dir) = ann_segment_dir(rt, model) else {
            bump_generation(ann, key).await;
            return FreshTailOutcome::Replace(
                Vec::new(),
                Some("fresh-tail: re-resolved segment directory unavailable; dropped stale candidates"),
            );
        };
        let bridge = match AnnBridge::load(&dir) {
            Ok(b) => b,
            Err(e) => {
                // The caller chose re-resolution from the delta HEAD alone. A
                // load that rejects the segment (a missing or corrupt chunk on
                // the chain HEAD names included) cannot deliver the watermark
                // HEAD promised. A write compacted into that chain is in
                // neither the stale candidates nor the log above the registry
                // minimum, so no tail can complete them: drop them.
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved segment load failed; dropping stale ANN candidates");
                bump_generation(ann, key).await;
                return FreshTailOutcome::Replace(
                    Vec::new(),
                    Some("fresh-tail: re-resolved segment load failed; dropped stale candidates"),
                );
            }
        };
        let s_loaded = bridge.index.last_applied_seq().unwrap_or(expected_s);
        let candidates = match bridge.search_with_route(query, k, route) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved segment search failed; dropping stale ANN candidates");
                bump_generation(ann, key).await;
                return FreshTailOutcome::Replace(
                    Vec::new(),
                    Some("fresh-tail: re-resolved segment search failed; dropped stale candidates"),
                );
            }
        };
        // This load served only the current query; force re-adoption so the
        // background warm path installs it for future ones too.
        bump_generation(ann, key).await;

        #[cfg(test)]
        {
            if ann
                .reresolve_race_barrier
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                ann.reresolve_race_notify.notify_one();
                ann.reresolve_race_release.notified().await;
            }
        }

        // Re-validate the registry minimum and run the tail scan inside one
        // snapshot so a concurrent checkpoint can't compact past `s_loaded`
        // between the load above and this fetch (docs/ann.md).
        let mut reader = match rt.sql().reader().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved reader open failed; serving re-resolved candidates without further tail");
                return FreshTailOutcome::Replace(
                    candidates,
                    Some("fresh-tail: re-resolved reader open failed"),
                );
            }
        };
        if let Err(e) = begin_read_snapshot(reader.as_mut()).await {
            tracing::warn!(error = %e, model, "fresh-tail: re-resolved snapshot begin failed; serving re-resolved candidates without further tail");
            return FreshTailOutcome::Replace(
                candidates,
                Some("fresh-tail: re-resolved snapshot begin failed"),
            );
        }
        if let Some(consumer) = consumer {
            let own = consumer_watermark_on(reader.as_mut(), model, consumer).await;
            if !matches!(own, Ok(Some(watermark)) if watermark >= 0) {
                end_read_snapshot(reader.as_mut()).await;
                return FreshTailOutcome::Skipped(SkipReason::bare(
                    "note-search ANN consumer is not active after segment re-resolution",
                ));
            }
        }

        let registry_min = match registry_min_watermark_on(reader.as_mut(), model).await {
            Ok(v) => v.and_then(|value| u64::try_from(value).ok()),
            Err(e) => {
                end_read_snapshot(reader.as_mut()).await;
                tracing::warn!(error = %e, model, "fresh-tail: re-resolved registry-min read failed; serving re-resolved candidates without further tail");
                return FreshTailOutcome::Replace(
                    candidates,
                    Some("fresh-tail: re-resolved registry-min read failed"),
                );
            }
        };

        let coherent = match registry_min {
            Some(m) => m <= s_loaded,
            None => true,
        };
        if coherent {
            // Coherent (these candidates, this watermark) pair: the log
            // still retains every row above `s_loaded`, so the tail scan
            // needs no floor beyond it.
            let outcome = fetch_final_tail_on(reader.as_mut(), model, s_loaded, None).await;
            end_read_snapshot(reader.as_mut()).await;
            return match outcome {
                Ok((ops, _)) => replace_with_merged_tail(candidates, query, ops, route),
                Err(e) => {
                    tracing::warn!(error = %e, model, "fresh-tail: re-resolved tail fetch failed; serving re-resolved candidates without further tail");
                    FreshTailOutcome::Replace(
                        candidates,
                        Some("fresh-tail: re-resolved tail fetch failed"),
                    )
                }
            };
        }
        let m = registry_min.expect("coherent=false implies registry_min is Some");

        if round == FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
            // Terminal round (ADR-118 §1): serve the last loaded candidates
            // floored at the last observed minimum — coherent, at the cost
            // of the (s_loaded, m] window not being provably retained.
            tracing::warn!(
                model,
                rounds = round,
                floor = m,
                "fresh-tail: re-resolution did not converge within {round} rounds; \
                 serving the ADR-118 floored fallback"
            );
            let outcome = fetch_final_tail_on(reader.as_mut(), model, m, None).await;
            end_read_snapshot(reader.as_mut()).await;
            return match outcome {
                Ok((ops, _)) => replace_with_merged_tail(candidates, query, ops, route),
                Err(e) => {
                    tracing::warn!(error = %e, model, "fresh-tail: floored-fallback tail fetch failed; serving re-resolved candidates without further tail");
                    FreshTailOutcome::Replace(
                        candidates,
                        Some("fresh-tail: floored-fallback tail fetch failed"),
                    )
                }
            };
        }

        end_read_snapshot(reader.as_mut()).await;
        // Reload the currently published segment next round — by the
        // compaction invariant above it is now at or past `m`.
        expected_s = m;
    }
    unreachable!("the loop always returns on or before its terminal round")
}

/// Tier 2 (§3): no serving index at all. A cheap, log-only existence probe
/// (no corpus join) fast-paths the common empty-tail case; a non-empty tail
/// is capped at ceil(`ann_rebuild_threshold() * live corpus`) rows. After the
/// probe, one statement/snapshot computes that live count, selects the newest
/// suffix, and hydrates its current vectors and note liveness.
async fn fresh_tail_capped(rt: &KhiveRuntime, model: &str) -> FreshTailOutcome {
    fresh_tail_capped_at_threshold(rt, model, ann_rebuild_threshold()).await
}

async fn fresh_tail_capped_at_threshold(
    rt: &KhiveRuntime,
    model: &str,
    live_threshold: f64,
) -> FreshTailOutcome {
    match tail_exists(rt, model, 0).await {
        Ok(false) => return FreshTailOutcome::Ops(Vec::new()),
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: tail-existence read failed; skipping capped exact leg");
            return FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: tail-existence read failed",
                e,
            ));
        }
    }
    match fetch_final_tail(rt, model, 0, Some(live_threshold)).await {
        Ok((ops, _new_s)) => FreshTailOutcome::Ops(ops),
        Err(e) => {
            tracing::warn!(error = %e, model, "fresh-tail: capped tail fetch failed; skipping exact leg");
            FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: capped tail fetch failed",
                e,
            ))
        }
    }
}

#[derive(Clone, Copy)]
struct CheckpointPublication {
    generation: u64,
    epoch: u64,
    authority: WatermarkAuthority,
}

enum CheckpointResult {
    Full {
        reopened: Option<Box<AnnBridge>>,
        base_digest: [u8; 32],
    },
    Delta(delta::DeltaPublication),
}

enum WrittenCheckpoint {
    Full([u8; 32]),
    Delta(delta::DeltaPublication),
}

/// Persist `bridge`, raise the wildcard registry row, compact the log, then
/// reopen the just-written segment via mmap and swap it in for the Owned
/// build product (ADR-079 Amendment 1 §B; see `docs/ann.md`). A failed
/// persistence or fenced publication never installs the candidate.
/// In-memory backends install the Owned candidate before raising/compacting
/// instead, since they have no segment to reopen.
async fn checkpoint_raise_compact_readopt(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    mut bridge: AnnBridge,
    publication: CheckpointPublication,
) -> bool {
    let CheckpointPublication {
        generation,
        epoch,
        authority,
    } = publication;
    let applied = bridge.index.last_applied_seq().unwrap_or(0);
    let namespace_set = bridge.namespace_set.clone();
    let stamp =
        |b: AnnBridge| -> AnnBridge { b.with_generation(generation).with_epoch_baseline(epoch) };

    let Some(dir) = ann_segment_dir(rt, model) else {
        // No filesystem commit record exists to re-resolve against, so
        // publish the bridge first; fresh-tail's registry check rejects and
        // evicts it until the conditional raise below succeeds.
        bridge.mark_checkpointed();
        if !install_replacing(ann, key, stamp(bridge)).await {
            // A post-scan generation already installed something newer. Do
            // not advance the registry past log rows that rejected candidate
            // may not contain; retry from the ordinary freshness path.
            return false;
        }
        if let Err(e) = raise_watermark_with_authority(rt, model, applied, authority).await {
            tracing::warn!(error = %e, "memory ann watermark publication rejected; dropping candidate bridge");
            evict_unprotected_index(ann, key).await;
            return false;
        }
        advance_note_search_watermark(rt, ann, model, applied, authority).await;
        if let Err(e) = compact_log(rt, model).await {
            tracing::warn!(error = %e, "memory ann log compaction failed (retries next checkpoint)");
        }
        return true;
    };

    match persist_file_checkpoint(rt, ann, model, &dir, &bridge, authority).await {
        Ok(CheckpointResult::Full {
            reopened,
            base_digest,
        }) => {
            let mut replacement = match reopened {
                Some(reopened) => *reopened,
                None => {
                    bridge.mark_full_checkpoint_base(base_digest);
                    bridge
                }
            };
            replacement.mark_checkpointed();
            replacement.set_namespace_set(namespace_set);
            install_replacing(ann, key, stamp(replacement)).await
        }
        Ok(CheckpointResult::Delta(publication)) => {
            bridge.mark_delta_checkpoint(&publication);
            bridge.mark_checkpointed();
            bridge.set_namespace_set(namespace_set);
            install_replacing(ann, key, stamp(bridge)).await
        }
        Err(unprotected) => {
            if unprotected {
                evict_unprotected_index(ann, key).await;
            }
            false
        }
    }
}

/// Publish through the existing registry fence while borrowing the candidate.
/// A read guard can keep an incremental incumbent available throughout file I/O.
/// `Err(true)` means registry protection was lost; other failures retain it.
async fn persist_file_checkpoint(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
    dir: &std::path::Path,
    bridge: &AnnBridge,
    authority: WatermarkAuthority,
) -> Result<CheckpointResult, bool> {
    let applied = bridge.index.last_applied_seq().unwrap_or(0);
    // Every process writing this model's segment takes the same filesystem
    // lock. Revalidate the durable row only after acquiring it: otherwise a
    // stale publisher could overwrite a newer segment, lose its conditional
    // raise, and leave the registry ahead of the files that restart adopts.
    let _publication_lock = match acquire_bridge_checkpoint_lock_async(dir.to_path_buf()).await {
        Ok(lock) => lock,
        Err(e) => {
            tracing::warn!(error = %e, "failed to acquire memory ANN checkpoint lock");
            return Err(false);
        }
    };
    let current_watermark = match read_own_watermark(rt, model).await {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(error = %e, "memory ANN checkpoint registry read failed");
            return Err(false);
        }
    };
    let authorized = match authority {
        WatermarkAuthority::PendingOrActive => {
            current_watermark == Some(PENDING_WATERMARK)
                || current_watermark.is_some_and(|watermark| {
                    watermark >= 0 && u64::try_from(watermark).is_ok_and(|value| value <= applied)
                })
        }
        WatermarkAuthority::Active => current_watermark.is_some_and(|watermark| {
            watermark >= 0 && u64::try_from(watermark).is_ok_and(|value| value <= applied)
        }),
        WatermarkAuthority::Recovering => {
            current_watermark == Some(ann_registry::RECOVERING_WATERMARK)
        }
    };
    if !authorized {
        tracing::info!(
            model,
            candidate_watermark = applied,
            observed_watermark = ?current_watermark,
            ?authority,
            "memory ANN checkpoint lost publication race before persistence"
        );
        return Err(false);
    }

    // A peer may have published a different overlay at the same SQL watermark.
    // The registry alone cannot distinguish that case: preserve the exact
    // segment + delta identity this candidate was built from.
    if let Some(expected) = bridge.commit_digest {
        match delta::publication_digest(dir) {
            Ok(Some(observed)) if observed == expected => {}
            Ok(observed) => {
                tracing::info!(
                    model,
                    ?observed,
                    "memory ANN checkpoint base changed before publication"
                );
                return Err(false);
            }
            Err(error) => {
                tracing::warn!(%error, model, "memory ANN checkpoint identity read failed");
                return Err(false);
            }
        }
    }

    let persisted = write_file_checkpoint(dir, bridge, delta::MAX_RETIRED_CHAIN);
    let written = match persisted {
        Ok(publication) => publication,
        Err(e) => {
            tracing::error!(error = %e, "failed to persist memory ANN checkpoint");
            // An ordinary active rebuild still has a registry-protected incumbent
            // and a retained tail. Preserve that stale fallback until a complete
            // replacement commits; pending/closed paths already evicted before
            // entering the scan and therefore have nothing unsafe to retain.
            return Err(false);
        }
    };
    #[cfg(test)]
    ann.publication_count.fetch_add(1, Ordering::SeqCst);
    if let Err(e) = raise_watermark_with_authority(rt, model, applied, authority).await {
        // Retirement or a newer checkpoint won the writer race.  This build's
        // candidates are not protected by its own registry state, so never
        // install them; the next ensure re-resolves durable state.
        tracing::warn!(error = %e, "memory ann watermark publication rejected; dropping candidate bridge");
        return Err(true);
    }
    advance_note_search_watermark(rt, ann, model, applied, authority).await;
    if let Err(e) = compact_log(rt, model).await {
        tracing::warn!(error = %e, "memory ann log compaction failed (retries next checkpoint)");
    }
    match written {
        WrittenCheckpoint::Delta(publication) => Ok(CheckpointResult::Delta(publication)),
        WrittenCheckpoint::Full(base_digest) => match load_segment(ann, dir) {
            Ok(mmap_bridge) => Ok(CheckpointResult::Full {
                reopened: Some(Box::new(mmap_bridge)),
                base_digest,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "memory ann mmap re-adoption failed; serving Owned build");
                Ok(CheckpointResult::Full {
                    reopened: None,
                    base_digest,
                })
            }
        },
    }
}

fn write_file_checkpoint(
    dir: &std::path::Path,
    bridge: &AnnBridge,
    max_chunks: usize,
) -> Result<WrittenCheckpoint, String> {
    if bridge.needs_full_compaction_with_chain_limit(max_chunks) {
        bridge.save_atomic(dir).map(WrittenCheckpoint::Full)
    } else {
        delta::write(dir, bridge).map(WrittenCheckpoint::Delta)
    }
}

/// Outcome of the v2-segment decision table for this consumer's global scope.
enum SegmentOutcome {
    /// An index was installed; carries the status the ensure path reports.
    Installed(AnnEnsureStatus),
    /// Live corpus is zero: no ANN candidate may be served or replayed
    /// (decision rule 5).
    Empty,
    /// No trustworthy segment: fall through to the rebuild path.
    Cold,
}

fn effective_persisted_state(dir: &std::path::Path, base_seq: u64) -> Result<(u64, u64), String> {
    let base_digest = segment_commit_digest(dir)?
        .ok_or_else(|| "memory ANN segment commit vanished".to_string())?;
    Ok(delta::read_info(dir, &base_digest, base_seq)?.unwrap_or((base_seq, 0)))
}

/// ADR-079 Amendment 1 restart classifier: the 8-rule first-match decision
/// table for the memory pack's global-scope note index, followed by the
/// matching adoption action. Full table and rationale: `docs/ann.md`.
#[allow(clippy::too_many_arguments)]
async fn classify_and_adopt_segment(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    seg_dir: &std::path::Path,
    target_generation: u64,
    target_epoch: u64,
    details: &mut AnnWarmDetails,
) -> SegmentOutcome {
    // Rule 1: commit record absent, corrupt, or invalid length → Cold.
    let info = match read_commit_info(seg_dir) {
        Ok(Some(info)) => info,
        Ok(None) => return SegmentOutcome::Cold,
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "error reading memory v2 commit record; Cold");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 2: readable but pre-amendment (no watermark) → Cold.
    let Some(base_seq) = info.last_applied_seq else {
        tracing::info!(model = %model,
            "pre-amendment memory v2 segment (no watermark); Cold rebuild");
        return SegmentOutcome::Cold;
    };
    let (s, _persisted_delta_ops) = match effective_persisted_state(seg_dir, base_seq) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, model, "memory delta commit is invalid; Cold rebuild");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 3: configured embedder dimensions ≠ segment dimensions → Cold.
    // Read from embedder configuration, not the corpus — no storage I/O.
    match rt.embedder_dimensions(model) {
        Some(dims) if dims as u64 == info.dimensions => {}
        Some(dims) => {
            tracing::info!(model = %model,
                segment_dims = info.dimensions, live_dims = dims,
                "memory v2 segment dimension mismatch; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        None => return SegmentOutcome::Cold,
    }

    // Rule 4: own wildcard registry row absent for an extended-format state →
    // Cold after re-registering as pending.
    match read_own_watermark(rt, model).await {
        Ok(Some(watermark)) if u64::try_from(watermark).is_ok_and(|watermark| watermark > s) => {
            tracing::warn!(
                model,
                watermark,
                persisted_seq = s,
                "memory ANN publication is behind its active registry watermark; Cold rebuild"
            );
            return SegmentOutcome::Cold;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::info!(model = %model,
                "memory ann consumer registry row absent; re-registering pending, Cold rebuild");
            if let Err(e) = register_consumer(rt, model).await {
                tracing::warn!(error = %e, "memory ann consumer re-registration failed");
            }
            return SegmentOutcome::Cold;
        }
        Err(e) => {
            tracing::warn!(error = %e, "memory ann registry read failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // Rule 6, tested before rule 5 (docs/ann.md: "Evaluation order of rules
    // 5 and 6"): no tail above S → Hot, mmap load with zero corpus I/O. The
    // namespace set stays empty (conservative default) rather than paying an
    // O(N) DISTINCT corpus scan.
    match tail_exists(rt, model, s).await {
        Ok(false) => {
            return match load_segment(ann, seg_dir) {
                Ok(bridge) => {
                    let bridge = bridge
                        .with_generation(target_generation)
                        .with_epoch_baseline(target_epoch);
                    install_replacing(ann, key, bridge).await;
                    details.path = "segment_load";
                    tracing::debug!(model = %model, "memory ANN loaded Hot from v2 segment");
                    SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot)
                }
                Err(e) => {
                    tracing::warn!(error = %e, dir = %seg_dir.display(),
                        "memory Hot segment load failed; Cold rebuild");
                    SegmentOutcome::Cold
                }
            };
        }
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, "memory ann tail probe failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // A tail exists: rules 5, 7, and 8 need (live, tail) from one snapshot.
    let (live, tail) = match scope_counts(rt, model, s).await {
        Ok(counts) => counts,
        Err(e) => {
            tracing::warn!(error = %e, "memory ann scope-count read failed; Cold");
            return SegmentOutcome::Cold;
        }
    };
    #[cfg(test)]
    ann.pause_stale_tail_scope_for_test().await;

    // Rule 5: zero live corpus → Empty, regardless of tail contents.
    if live == 0 {
        return SegmentOutcome::Empty;
    }

    // Rule 7: apply ADR-079's raw-tail work limit (see its default rationale),
    // independently of delta-chain headroom. A replay that reaches the chain limit
    // publishes a full checkpoint after applying the tail.
    let threshold = replay_limit(live, ann_rebuild_threshold());
    if tail <= threshold {
        let mut bridge = match load_segment(ann, seg_dir) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, dir = %seg_dir.display(),
                    "memory Stale-tail segment load failed; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        // The earlier scope count selected this branch, but writes can land
        // before replay. Read the actual operations, raw count, and terminal
        // watermark together so the published chunk describes one snapshot.
        let IncrementalTail {
            ops,
            applied: new_s,
            raw_count,
        } = match protected_tail(rt, ann, model, s, threshold).await {
            Ok(Some(tail)) => tail,
            Ok(None) => {
                tracing::info!(model, "memory tail grew past replay cap; Cold rebuild");
                return SegmentOutcome::Cold;
            }
            Err(e) => {
                tracing::warn!(error = %e, "memory tail replay read failed; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        details.ops_applied = ops.len() as u64;
        let recorded_ops = ops.clone();
        if let Err(e) = bridge.apply_final_ops(ops, new_s) {
            tracing::warn!(error = %e, "memory tail replay failed; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        bridge.record_delta_batch(recorded_ops, new_s, raw_count);
        // Replay is cheap and in memory; the checkpoint publishes a delta
        // below the compaction bound. A non-owner serves the replayed bridge
        // without publication, so another client does not rewrite the segment.
        if !ann.builds_corpus_indexes {
            details.path = "stale_tail_replay";
            install_replacing(
                ann,
                key,
                bridge
                    .with_generation(target_generation)
                    .with_epoch_baseline(target_epoch),
            )
            .await;
            tracing::debug!(model = %model, tail,
                "memory ANN served from Stale-tail replay without checkpoint; not the warm index host");
            return SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot);
        }
        details.path = "stale_tail_publication";
        if bridge.needs_full_compaction() {
            if let Err(error) = bridge.consolidate_if_needed(checkpoint_policy(ann).consolidate_tau)
            {
                tracing::warn!(%error, "memory ANN consolidation failed before replay publication");
                return SegmentOutcome::Cold;
            }
        }
        let installed = checkpoint_raise_compact_readopt(
            rt,
            ann,
            key,
            model,
            bridge,
            CheckpointPublication {
                generation: target_generation,
                epoch: target_epoch,
                authority: WatermarkAuthority::Active,
            },
        )
        .await;
        if !installed {
            return SegmentOutcome::Cold;
        }
        tracing::debug!(model = %model, tail, "memory ANN adopted via Stale-tail replay");
        return SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot);
    }

    // Rule 8: tail above threshold → Stale-rebuild: serve the checksum-valid
    // segment while the caller's rebuild path replaces it. Cost decision,
    // never a demotion to FTS-only.
    match load_segment(ann, seg_dir) {
        Ok(bridge) => {
            tracing::info!(model = %model, tail, live,
                "memory tail above rebuild threshold; serving stale segment during rebuild");
            let bridge = bridge
                .with_generation(target_generation)
                .with_epoch_baseline(target_epoch);
            install_replacing(ann, key, bridge).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "memory Stale-rebuild segment load failed; rebuilding without serve-stale");
        }
    }
    SegmentOutcome::Cold
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ann_owned_build_tests.rs"]
mod owned_build_tests;

#[cfg(test)]
mod tests {
    use super::*;
    mod incremental_tests;
    mod maintenance_lock_tests;
    use serial_test::serial;

    #[tokio::test(start_paused = true)]
    #[serial(background_tasks)]
    async fn rotation_watcher_exits_on_local_shutdown_without_advancing_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(dir.path().join("rotation-shutdown.db")),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("writable runtime");
        let ann = new_shared();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let before = khive_runtime::background_task_count();
        let started_at = tokio::time::Instant::now();

        let watcher = start_rotation_watcher_with_shutdown(&rt, &ann, shutdown.clone())
            .expect("file-backed ANN state starts a watcher");
        assert!(rotation_watch_started_for_test(&ann));
        assert_eq!(khive_runtime::background_task_count(), before + 1);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(khive_runtime::background_task_count(), before + 1);

        assert!(
            !watcher.is_finished(),
            "uncancelled watcher must stay active"
        );

        shutdown.cancel();
        for _ in 0..100 {
            if watcher.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        if !watcher.is_finished() {
            watcher.abort();
            let _ = watcher.await;
            panic!("local shutdown must stop the watcher while its ANN state is alive");
        }
        watcher
            .await
            .expect("rotation watcher must finish successfully after local shutdown");
        assert_eq!(khive_runtime::background_task_count(), before);
        assert_eq!(Arc::strong_count(&ann), 1);
        assert_eq!(tokio::time::Instant::now(), started_at);
    }

    /// Owns a file-backed runtime and removes its database directory after shutdown.
    struct TestRuntime {
        runtime: KhiveRuntime,
        _temp_dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestRuntime {
        type Target = KhiveRuntime;

        fn deref(&self) -> &Self::Target {
            &self.runtime
        }
    }

    struct InterleavingTailReader {
        subject: Uuid,
        calls: Vec<String>,
    }

    impl InterleavingTailReader {
        fn row(columns: Vec<(&str, SqlValue)>) -> khive_storage::SqlRow {
            khive_storage::SqlRow {
                columns: columns
                    .into_iter()
                    .map(|(name, value)| khive_storage::types::SqlColumn {
                        name: name.to_owned(),
                        value,
                    })
                    .collect(),
            }
        }

        fn embedding(values: &[f32]) -> SqlValue {
            SqlValue::Blob(
                values
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect(),
            )
        }
    }

    #[async_trait::async_trait]
    impl khive_storage::SqlReader for InterleavingTailReader {
        async fn query_row(
            &mut self,
            _statement: SqlStatement,
        ) -> khive_storage::StorageResult<Option<khive_storage::SqlRow>> {
            panic!("fresh-tail replay must use query_all")
        }

        async fn query_all(
            &mut self,
            statement: SqlStatement,
        ) -> khive_storage::StorageResult<Vec<khive_storage::SqlRow>> {
            let label = statement.label.unwrap_or_default();
            self.calls.push(label.clone());
            let id = SqlValue::Text(self.subject.to_string());
            Ok(match label.as_str() {
                // The coherent snapshot: the suffix and its matching current
                // vector are both the pre-commit state.
                "memory_ann_fresh_tail_snapshot" => {
                    assert!(
                        statement.sql.contains("COUNT(*) AS live_count"),
                        "the snapshot statement must derive the corpus-relative cap"
                    );
                    assert!(
                        statement.sql.contains("LIMIT (SELECT"),
                        "the snapshot statement must apply the derived newest-suffix cap"
                    );
                    assert!(
                        statement.sql.contains("LEFT JOIN vec_snapshot_race_model"),
                        "the snapshot statement must hydrate the selected suffix's vectors"
                    );
                    assert!(
                        statement.sql.contains("LEFT JOIN notes"),
                        "the snapshot statement must evaluate note liveness"
                    );
                    vec![Self::row(vec![
                        ("seq", SqlValue::Integer(1)),
                        ("subject_id", id),
                        ("op", SqlValue::Text("upsert".into())),
                        ("vector_model", SqlValue::Text("snapshot-race-model".into())),
                        ("vector_kind", SqlValue::Text("note".into())),
                        ("vector_field", SqlValue::Text("note.content".into())),
                        ("embedding", Self::embedding(&[1.0, 0.0])),
                        ("live_note_id", SqlValue::Text(self.subject.to_string())),
                    ])]
                }
                // Legacy multi-query behavior: a writer commits immediately
                // after suffix selection, so the next pooled read observes a
                // newer vector than the selected log row described.
                "memory_ann_fetch_tail" => vec![Self::row(vec![
                    ("seq", SqlValue::Integer(1)),
                    ("subject_id", id),
                    ("op", SqlValue::Text("upsert".into())),
                ])],
                "memory_ann_tail_point_read" => vec![Self::row(vec![
                    (
                        "embedding_model",
                        SqlValue::Text("snapshot-race-model".into()),
                    ),
                    ("kind", SqlValue::Text("note".into())),
                    ("field", SqlValue::Text("note.content".into())),
                    ("embedding", Self::embedding(&[0.0, 1.0])),
                ])],
                "memory_ann_tail_live_notes" => vec![Self::row(vec![(
                    "id",
                    SqlValue::Text(self.subject.to_string()),
                )])],
                other => panic!("unexpected fresh-tail query label: {other}"),
            })
        }

        async fn query_scalar(
            &mut self,
            _statement: SqlStatement,
        ) -> khive_storage::StorageResult<Option<SqlValue>> {
            panic!("fresh-tail replay must use query_all")
        }

        async fn explain(
            &mut self,
            _statement: SqlStatement,
        ) -> khive_storage::StorageResult<Vec<khive_storage::SqlRow>> {
            panic!("fresh-tail replay must not issue EXPLAIN")
        }
    }

    /// A pool-backed reader must see a racing commit as entirely visible or entirely invisible, never mixed.
    #[tokio::test]
    async fn fresh_tail_snapshot_cannot_return_a_torn_log_vector_pair() {
        let subject = Uuid::new_v4();
        let mut reader = InterleavingTailReader {
            subject,
            calls: Vec::new(),
        };

        let (ops, watermark) =
            fetch_final_tail_on(&mut reader, "snapshot-race-model", 0, Some(0.20))
                .await
                .expect("fresh-tail snapshot");

        assert_eq!(watermark, 1);
        assert_eq!(ops, vec![(subject, Some(vec![1.0, 0.0]))]);
        assert_eq!(
            reader.calls,
            vec!["memory_ann_fresh_tail_snapshot"],
            "one logical no-index replay must execute exactly one SQLite statement"
        );
    }

    #[test]
    fn outcome_into_candidates_replace_with_reason_discloses_degradation() {
        // A reasoned Replace must surface its failure-site reason, not report healthy.
        let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
        let replaced = vec![(Uuid::from_u128(2), 0.8_f64)];
        let (candidates, disclosure) = outcome_into_candidates(
            FreshTailOutcome::Replace(
                replaced.clone(),
                Some("fresh-tail: re-resolved tail fetch failed"),
            ),
            prior,
            &[1.0, 0.0],
        );
        assert_eq!(
            candidates,
            vec![(Uuid::from_u128(2), 0.8_f64 as f32)],
            "Replace must swap the candidate set"
        );
        assert_eq!(
            disclosure.as_deref(),
            Some("fresh-tail: re-resolved tail fetch failed"),
            "a reasoned Replace must disclose its failure site"
        );
    }

    #[test]
    fn outcome_into_candidates_healthy_replace_and_ops_do_not_disclose() {
        let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
        let replaced = vec![(Uuid::from_u128(2), 0.8_f64)];
        let (candidates, disclosure) = outcome_into_candidates(
            FreshTailOutcome::Replace(replaced.clone(), None),
            prior.clone(),
            &[1.0, 0.0],
        );
        assert_eq!(candidates, vec![(Uuid::from_u128(2), 0.8_f64 as f32)]);
        assert!(
            disclosure.is_none(),
            "a fully assembled re-resolution is not degraded"
        );

        let (candidates, disclosure) = outcome_into_candidates(
            FreshTailOutcome::Ops(Vec::new()),
            prior.clone(),
            &[1.0, 0.0],
        );
        assert_eq!(candidates, prior);
        assert!(disclosure.is_none(), "an empty tail merge is not degraded");
    }

    #[test]
    fn outcome_into_candidates_skipped_keeps_prior_and_discloses() {
        let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
        let (candidates, disclosure) = outcome_into_candidates(
            FreshTailOutcome::Skipped(SkipReason::with_error(
                "fresh-tail: reader open failed",
                "pool exhausted after 5s",
            )),
            prior.clone(),
            &[1.0, 0.0],
        );
        assert_eq!(candidates, prior, "Skipped must leave candidates untouched");
        assert_eq!(
            disclosure.as_deref(),
            Some("fresh-tail: reader open failed: pool exhausted after 5s"),
            "the disclosure must carry the error that caused the skip, not \
             only the label shared by every failure at that site"
        );
    }

    /// A site holding no error keeps emitting the bare label: the enriched
    /// rendering must not smuggle a separator or a placeholder onto a skip
    /// that genuinely has nothing further to say.
    #[test]
    fn skip_reason_without_an_error_renders_the_bare_label() {
        const LABEL: &str = "note-search ANN consumer is not active in tail snapshot";
        let reason = SkipReason::bare(LABEL);
        assert_eq!(reason.detail(), None);
        assert_eq!(reason.label(), LABEL);
        assert_eq!(reason.to_string(), LABEL);

        let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
        let (candidates, disclosure) = outcome_into_candidates(
            FreshTailOutcome::Skipped(SkipReason::bare(LABEL)),
            prior.clone(),
            &[1.0, 0.0],
        );
        assert_eq!(candidates, prior);
        assert_eq!(
            disclosure.as_deref(),
            Some(LABEL),
            "an error-free skip must disclose exactly the label it always did"
        );
    }

    /// Two skips that share a label are told apart by the error each carries:
    /// an exhausted reader pool is retryable, an unopenable database file is
    /// not, and the label alone cannot separate them.
    #[test]
    fn skip_reason_separates_two_causes_that_share_a_label() {
        const LABEL: &str = "fresh-tail: reader open failed";
        let exhausted = SkipReason::with_error(LABEL, "pool exhausted after 5s");
        let unopenable = SkipReason::with_error(LABEL, "unable to open database file");

        assert_eq!(exhausted.label(), unopenable.label());
        assert_ne!(
            exhausted.to_string(),
            unopenable.to_string(),
            "the two causes must be distinguishable in the served reason"
        );
        for (reason, expected_error) in [
            (&exhausted, "pool exhausted after 5s"),
            (&unopenable, "unable to open database file"),
        ] {
            let rendered = reason.to_string();
            assert!(
                rendered.starts_with(LABEL),
                "the label must stay at the front of the reason, got: {rendered:?}"
            );
            assert!(
                rendered.contains(expected_error),
                "the reason must carry the error text, got: {rendered:?}"
            );
        }
    }

    /// An unbounded error (a driver message quoting a whole statement) is cut
    /// to the documented character bound and marked as cut, so one degraded
    /// response cannot be bloated by the error it discloses.
    #[test]
    fn skip_reason_detail_is_bounded_and_marks_the_cut() {
        let long_error = "x".repeat(SKIP_DETAIL_MAX_CHARS * 10);
        let reason = SkipReason::with_error("fresh-tail: tail fetch failed", &long_error);

        let detail = reason
            .detail()
            .expect("an error-bearing skip carries detail");
        assert_eq!(
            detail.chars().count(),
            SKIP_DETAIL_MAX_CHARS,
            "a cut detail must land exactly on the bound, marker included"
        );
        assert!(
            detail.ends_with(SKIP_DETAIL_TRUNCATION_MARKER),
            "a cut must be marked so the reader knows the error continues, got: {detail:?}"
        );
        assert!(
            reason
                .to_string()
                .starts_with("fresh-tail: tail fetch failed"),
            "truncating the error must not disturb the label"
        );

        // The control arm: an error that fits is carried whole, with no
        // marker — otherwise the assertion above passes on a function that
        // simply truncates everything.
        let short_error = "y".repeat(SKIP_DETAIL_MAX_CHARS);
        let short = SkipReason::with_error("fresh-tail: tail fetch failed", &short_error);
        assert_eq!(
            short.detail(),
            Some(short_error.as_str()),
            "an error within the bound must be carried unchanged"
        );
    }

    /// The bound counts characters, not bytes: a multi-byte error message
    /// must never be cut mid-character (which would not even be a `String`).
    #[test]
    fn skip_reason_detail_bound_never_splits_a_character() {
        let multibyte = "\u{00e9}".repeat(SKIP_DETAIL_MAX_CHARS * 2);
        let bounded = bound_skip_detail(&multibyte);
        assert_eq!(bounded.chars().count(), SKIP_DETAIL_MAX_CHARS);
        assert!(bounded.ends_with(SKIP_DETAIL_TRUNCATION_MARKER));
        assert!(
            bounded
                .trim_end_matches(SKIP_DETAIL_TRUNCATION_MARKER)
                .chars()
                .all(|c| c == '\u{00e9}'),
            "the kept prefix must be whole characters, got: {bounded:?}"
        );
    }

    #[test]
    fn ann_key_is_model_only() {
        // After FTS+ANN consolidation AnnKey is model-only; namespace is ignored.
        let k1 = AnnKey::new("model-x");
        let k2 = AnnKey::new("model-x"); // same model, different ns → same key
        let k3 = AnnKey::new("model-y"); // different model → different key
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

    /// A stale id-map entry must not let a delete replay tombstone a slot a same-batch upsert reused (#1150).
    #[test]
    fn replay_does_not_tombstone_slot_reused_by_same_batch_upsert() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        let vectors = vec![
            1.0f32, 0.0, 0.0, // id_a, ordinal 0
            0.0, 1.0, 0.0, // id_b, ordinal 1
        ];
        let mut bridge =
            AnnBridge::build(vectors, 3, vec![id_a, id_b], HashSet::new()).expect("build");

        // Simulate a PRIOR tombstone of id_a that left the id-map entry
        // stale (tombstoning never clears it) — exactly the persisted state
        // #1150 describes, without going through a save/load round trip.
        bridge.index.tombstone(0).expect("tombstone id_a");
        assert_eq!(
            bridge.id_map[0], id_a,
            "id-map entry stays stale after tombstone"
        );

        // Coalesced final tail: id_c's upsert (which recycles id_a's freed
        // ordinal 0) is processed BEFORE id_a's own final delete — a legal
        // op order since coalescing only guarantees per-subject dedup, not
        // cross-subject sequencing.
        let ops = vec![(id_c, Some(vec![0.0f32, 0.0, 1.0])), (id_a, None)];
        bridge.apply_final_ops(ops, 1).expect("apply replay tail");

        assert_eq!(
            bridge.id_map[0], id_c,
            "ordinal 0 must be owned by id_c after the replay"
        );
        assert!(
            !bridge.index.is_tombstoned(0),
            "id_a's stale delete must not tombstone the slot id_c now owns"
        );
        let hits = bridge.search(&[0.0, 0.0, 1.0], 2).expect("search");
        assert!(
            hits.iter().any(|(id, score)| *id == id_c && *score > 0.9),
            "id_c must remain live and searchable, got: {hits:?}"
        );
        assert!(
            !hits.iter().any(|(id, _)| *id == id_a),
            "id_a must not resurface as a search hit, got: {hits:?}"
        );
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

    // Writes preserve the installed graph and persisted segment until a fresher
    // build replaces them.

    #[tokio::test]
    async fn bump_generation_does_not_evict_installed_index_or_segment() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");
        let id = Uuid::new_v4();

        install_replacing(&ann, &key, tiny_bridge(id, 1)).await;
        let seg_dir = tempfile::Builder::new()
            .prefix("khive-memory-ann-seg-")
            .tempdir_in(std::env::temp_dir())
            .expect("segment tempdir");
        {
            let idxs = ann.indexes.read().await;
            let bridge = idxs.get(&key).expect("installed above");
            bridge.save_atomic(seg_dir.path()).expect("persist segment");
        }

        // A write lands: it bumps the generation but must not clear anything.
        bump_generation(&ann, &key).await;

        assert!(
            ann.indexes.read().await.contains_key(&key),
            "a write must not evict the previously-installed in-memory index"
        );
        assert!(
            AnnBridge::load(seg_dir.path()).is_ok(),
            "a write must not invalidate the previously-persisted segment before \
             a fresher checkpoint has durably replaced it"
        );
    }

    /// `search_loaded` serves an installed stale graph rather than forcing an inline rebuild.
    #[tokio::test]
    async fn search_loaded_serves_stale_installed_entry_without_rebuild() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");
        let id = Uuid::new_v4();

        install_replacing(&ann, &key, tiny_bridge(id, 1)).await;
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

    /// A peer can rotate an otherwise byte-identical checkpoint while this
    /// process is completely idle. One watcher tick must adopt its new UUID
    /// sidecar and drop the bridge that owns the unlinked predecessor mmaps.
    #[tokio::test]
    async fn rotation_tick_releases_predecessor_and_adopts_identical_peer_checkpoint() {
        const MODEL: &str = "memory-rotation-release-test-model";
        let rt = test_runtime_with_hash_embedder(MODEL, 4);
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();

        tiny_bridge(old_id, 7)
            .save_atomic(&dir)
            .expect("persist first generation");
        let mut loaded = AnnBridge::load(&dir)
            .expect("load first mmap generation")
            .with_generation(7);
        let probe = Arc::new(());
        let dropped = Arc::downgrade(&probe);
        loaded.drop_probe = Some(probe);
        assert!(install_replacing(&ann, &key, loaded).await);
        let first_digest = ann
            .indexes
            .read()
            .await
            .get(&key)
            .and_then(|bridge| bridge.commit_digest)
            .expect("loaded bridge identity");

        // The vector bytes and Vamana lifecycle are identical. Only the ID
        // sidecar denotes the peer's newly published logical mapping.
        tiny_bridge(new_id, 7)
            .save_atomic(&dir)
            .expect("rotate identical checkpoint");

        refresh_rotated_segments_once(&rt, &ann).await;

        assert!(
            dropped.upgrade().is_none(),
            "replacing the cache entry must drop the predecessor mmap owner"
        );
        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("rotated bridge installed");
        assert_ne!(bridge.commit_digest, Some(first_digest));
        let hits = bridge
            .search(&[1.0, 0.0, 0.0, 0.0], 1)
            .expect("search replacement");
        assert_eq!(hits.first().map(|hit| hit.0), Some(new_id));
        assert_eq!(bridge.generation, 7, "local generation fence is preserved");
    }

    /// A peer's rotated checkpoint can cover namespaces this process never
    /// queried. Adopting it must not inherit the incumbent's narrower
    /// namespace_set — recall's over-fetch decision trusts an empty set as
    /// "assume non-visible namespaces exist" and a stale narrow set as
    /// "corpus fully accounted for", so carrying the incumbent's set forward
    /// would hide eligible memories from namespaces the peer's checkpoint
    /// added.
    #[tokio::test]
    async fn rotation_tick_resets_namespace_set_instead_of_inheriting_incumbent() {
        const MODEL: &str = "memory-rotation-namespace-reset-test-model";
        let rt = test_runtime_with_hash_embedder(MODEL, 4);
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();

        tiny_bridge(old_id, 7)
            .save_atomic(&dir)
            .expect("persist first generation");
        let mut incumbent = AnnBridge::load(&dir)
            .expect("load first mmap generation")
            .with_generation(7);
        // Simulate a bridge this process actually built: it only ever
        // observed namespace "ns-a".
        incumbent.set_namespace_set(HashSet::from(["ns-a".to_string()]));
        assert!(install_replacing(&ann, &key, incumbent).await);

        // The peer's rotated checkpoint carries data from a namespace this
        // process never saw.
        tiny_bridge(new_id, 7)
            .save_atomic(&dir)
            .expect("rotate peer checkpoint covering an unseen namespace");

        refresh_rotated_segments_once(&rt, &ann).await;

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("rotated bridge installed");
        assert!(
            bridge.namespace_set.is_empty(),
            "rotation must not carry the incumbent's namespace_set {:?} onto a peer \
             checkpoint of unknown namespace coverage; recall requires the conservative \
             empty set to keep over-fetching for eligible visible memories",
            bridge.namespace_set
        );
    }

    /// A process without corpus-build authority declines instead of scanning and
    /// publishing. The second half is the control: the same corpus, the same
    /// runtime, with the authority, builds — so the decline is caused by the role
    /// and not by a fixture that could not have built anyway.
    #[tokio::test]
    async fn a_process_that_does_not_build_declines_instead_of_scanning_the_corpus() {
        const MODEL: &str = "memory-non-building-process-declines-test-model";
        const DIMS: usize = 4;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "a note the daemon will index",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");

        let key = AnnKey::new(MODEL);
        let client = new_shared_for_role(false);
        let declined = ensure_ann_for_model(&rt, &token, &client, MODEL)
            .await
            .expect("ensure must not error, it must decline");
        assert!(
            matches!(declined, AnnEnsureStatus::DeclinedNotWarmHost),
            "a process without corpus-build authority must decline, got {declined:?}"
        );
        assert!(
            !client.indexes.read().await.contains_key(&key),
            "a decline must install nothing"
        );
        if let Some(seg_dir) = ann_segment_dir(&rt, MODEL) {
            assert!(
                !seg_dir.join("metadata.bin").exists(),
                "a decline must publish no segment"
            );
        }

        let host = new_shared();
        let built = ensure_ann_for_model(&rt, &token, &host, MODEL)
            .await
            .expect("control build");
        assert!(
            matches!(built, AnnEnsureStatus::Built { vectors: 1 }),
            "control: with the authority the same corpus builds, got {built:?}"
        );
    }

    /// The Stale-tail path replays in memory and then checkpoints the delta.
    /// A process without corpus-build
    /// authority must serve the replayed bridge and publish nothing, or every
    /// client warming after any write republishes the segment. The search for
    /// the tail note is the witness that the replay path ran rather than a Hot
    /// load of the seeded segment. The control is the same state warmed with
    /// the authority, which does checkpoint.
    #[tokio::test]
    async fn a_process_that_does_not_build_replays_the_tail_without_publishing() {
        const MODEL: &str = "memory-non-building-process-stale-tail-test-model";
        const DIMS: usize = 4;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..4 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("seeded note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seeded note");
        }
        let seed = new_shared();
        let built = ensure_ann_for_model(&rt, &token, &seed, MODEL)
            .await
            .expect("seed build");
        assert!(
            matches!(built, AnnEnsureStatus::Built { vectors: 4 }),
            "seed: expected a build over 4 vectors, got {built:?}"
        );
        let seg_dir = ann_segment_dir(&rt, MODEL).expect("segment dir");
        let metadata = seg_dir.join("metadata.bin");
        let vectors = seg_dir.join("vectors.bin");
        let before_metadata = std::fs::read(&metadata).expect("seeded metadata.bin");
        let before_vectors = std::fs::read(&vectors).expect("seeded vectors.bin");

        // One more note: live = 5, tail = 1 ≤ ceil(0.20 × 5) → Stale-tail.
        let tail_note = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "the note only a tail replay can find",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create tail note");

        let key = AnnKey::new(MODEL);
        let client = new_shared_for_role(false);
        let status = ensure_ann_for_model(&rt, &token, &client, MODEL)
            .await
            .expect("client warm");
        assert!(
            matches!(status, AnnEnsureStatus::LoadedSnapshot),
            "a client must adopt the segment through Stale-tail replay, got {status:?}"
        );
        let query = fnv_to_vec("the note only a tail replay can find", DIMS);
        let hits = search_loaded(&client, &key, &query, 5)
            .await
            .expect("search must succeed")
            .expect("the replayed bridge must be installed");
        assert!(
            hits.iter()
                .any(|(id, score)| *id == tail_note.id && *score > 0.99),
            "the tail note must be served from the replayed bridge, got {hits:?}"
        );
        assert_eq!(
            std::fs::read(&metadata).expect("metadata.bin after client warm"),
            before_metadata,
            "a client must not checkpoint: metadata.bin changed"
        );
        assert_eq!(
            std::fs::read(&vectors).expect("vectors.bin after client warm"),
            before_vectors,
            "a client must not checkpoint: vectors.bin changed"
        );

        let host = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &host, MODEL)
            .await
            .expect("host warm");
        assert!(
            matches!(status, AnnEnsureStatus::LoadedSnapshot),
            "control: the host adopts the same segment, got {status:?}"
        );
        assert_eq!(
            std::fs::read(&metadata).expect("metadata.bin after host warm"),
            before_metadata,
            "the host checkpoint must preserve the base segment nonce"
        );
        assert!(
            seg_dir.join(delta::HEAD_FILE).exists(),
            "the host must publish a delta HEAD after replay"
        );
    }

    /// The chain debounce is what decides how many writes one rebuild absorbs.
    /// One second absorbed nothing against a fleet writing continuously, which is
    /// how a coalescing window became a rebuild cadence.
    #[test]
    fn rebuild_chain_debounce_policy() {
        let default = std::time::Duration::from_secs(30);
        assert_eq!(resolve_rebuild_chain_debounce(None, default), default);
        assert_eq!(
            resolve_rebuild_chain_debounce(Some(" 2500 "), default),
            std::time::Duration::from_millis(2500)
        );
        // Zero is a real answer: it means no coalescing was asked for.
        assert_eq!(
            resolve_rebuild_chain_debounce(Some("0"), default),
            std::time::Duration::ZERO
        );
        // A malformed value must not silently become zero, which would restore
        // the behaviour this default exists to fix.
        assert_eq!(
            resolve_rebuild_chain_debounce(Some("soon"), default),
            default
        );
        assert_eq!(resolve_rebuild_chain_debounce(Some("-1"), default), default);
        assert_eq!(resolve_rebuild_chain_debounce(Some(""), default), default);
    }

    /// Mirrors the knowledge-pack invalid-rotation tests (issue #2340): a
    /// peer's rotated checkpoint that fails validation must evict the
    /// predecessor here too, and a later warm must recover. Unlike the
    /// knowledge pack, memory has no separate warm-lifecycle map to race —
    /// `refresh_rotated_segment` and `ensure_ann_for_model` both take
    /// `model_warm_lock` for the same key before touching `indexes`, so the
    /// watcher and an in-flight rebuild are
    /// already mutually exclusive; this test pins the recovery behavior that
    /// serialization is relied on to make safe.
    #[tokio::test]
    async fn invalid_rotation_evicts_predecessor_and_next_warm_recovers() {
        const MODEL: &str = "memory-invalid-rotation-recovery-test-model";
        const DIMS: usize = 4;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "invalid rotation recovery note",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");

        let ann = new_shared();
        let key = AnnKey::new(MODEL);

        let first = ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("initial full checkpoint");
        assert!(
            matches!(first, AnnEnsureStatus::Built { vectors: 1 }),
            "sanity: the initial warm must build and persist the corpus, got {first:?}"
        );
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "sanity: the initial build must be installed"
        );

        // A peer publishes a changed commit whose UUID sidecar is missing, so
        // the rotated generation fails validation and the incumbent is evicted.
        let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
        let incumbent_seq = ann
            .indexes
            .read()
            .await
            .get(&key)
            .and_then(|bridge| bridge.index.last_applied_seq())
            .expect("initial build carries an applied watermark");
        let mut rotated = tiny_bridge(Uuid::new_v4(), 1);
        rotated.set_applied_seq(incumbent_seq);
        rotated.save_atomic(&dir).expect("rotate checkpoint");
        std::fs::remove_file(dir.join("external_ids.bin")).expect("remove sidecar");

        refresh_rotated_segments_once(&rt, &ann).await;

        assert!(
            ann.indexes.read().await.get(&key).is_none(),
            "an invalid rotated generation must evict the incumbent"
        );

        // The next warm must recover: `model_warm_lock` is the same lock the
        // watcher just released, so no stale ownership blocks the rebuild.
        let recovered = ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("recovery checkpoint after eviction");
        assert!(
            matches!(recovered, AnnEnsureStatus::Built { vectors: 1 }),
            "the next warm must rebuild after the watcher's eviction, got {recovered:?}"
        );
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "the recovery build must reinstall the index"
        );
    }

    /// A strictly older generation than the installed entry must never replace it (pre-#750 bug shape).
    #[tokio::test]
    async fn install_replacing_rejects_older_generation_candidate() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");
        let newer_id = Uuid::new_v4();
        let older_id = Uuid::new_v4();

        assert!(install_replacing(&ann, &key, tiny_bridge(newer_id, 5)).await);
        assert!(!install_replacing(&ann, &key, tiny_bridge(older_id, 2)).await);

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("an entry must be installed");
        assert_eq!(bridge.generation, 5, "the newer generation must survive");
        assert_eq!(
            bridge.id_map,
            vec![newer_id],
            "the older-generation candidate must not have replaced it"
        );
    }

    /// A pathless build rejected by a newer post-scan generation must not advance the watermark or compact the tail.
    #[tokio::test]
    async fn pathless_rejected_candidate_does_not_raise_or_compact() {
        let rt = KhiveRuntime::memory().expect("runtime");
        let ann = new_shared();
        let model = "pathless-rejected-checkpoint";
        let key = AnnKey::new(model);
        register_consumer(&rt, model)
            .await
            .expect("register pending consumer");

        let newer_id = Uuid::new_v4();
        assert!(install_replacing(&ann, &key, tiny_bridge(newer_id, 5)).await);
        let mut rejected = tiny_bridge(Uuid::new_v4(), 2);
        rejected.set_applied_seq(2);

        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        for seq in 1..=2 {
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO ann_write_log \
                          (seq, namespace, embedding_model, kind, field, subject_id, op) \
                          VALUES (?1, 'local', ?2, 'note', 'note.content', ?3, 'upsert')"
                        .into(),
                    params: vec![
                        SqlValue::Integer(seq),
                        SqlValue::Text(model.into()),
                        SqlValue::Text(format!("subject-{seq}")),
                    ],
                    label: Some("test_pathless_checkpoint_tail".into()),
                })
                .await
                .expect("insert tail row");
        }
        drop(writer);

        assert!(
            !checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                model,
                rejected,
                CheckpointPublication {
                    generation: 2,
                    epoch: 0,
                    authority: WatermarkAuthority::PendingOrActive,
                },
            )
            .await,
            "a rejected generation must abort pathless publication"
        );
        assert_eq!(
            read_own_watermark(&rt, model)
                .await
                .expect("read watermark"),
            Some(PENDING_WATERMARK),
            "rejection must preserve the closed pending watermark"
        );
        let mut reader = sql.reader().await.expect("reader");
        let retained = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM ann_write_log WHERE embedding_model = ?1".into(),
                params: vec![SqlValue::Text(model.into())],
                label: Some("test_pathless_checkpoint_retained_tail".into()),
            })
            .await
            .expect("count retained tail");
        match retained {
            Some(SqlValue::Integer(2)) => {}
            other => panic!("rejected publication must retain both tail rows, got {other:?}"),
        }
        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("newer bridge remains installed");
        assert_eq!(bridge.generation, 5);
        assert_eq!(bridge.id_map, vec![newer_id]);
    }

    /// A pathless full scan after compaction must retain the active floor even though the log's MAX(seq) reset to zero.
    #[tokio::test]
    async fn pathless_full_checkpoint_inherits_compacted_active_floor() {
        const MODEL: &str = "pathless-compacted-active-floor";
        let rt = KhiveRuntime::memory().expect("runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: 4,
        });
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        for seq in 1..=2 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("pathless compacted-floor note {seq}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
            bump_generation(&ann, &key).await;
        }

        let first = ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("first full checkpoint");
        assert!(
            matches!(first, AnnEnsureStatus::Built { vectors: 2 }),
            "the first checkpoint must cover both writes, got {first:?}"
        );
        assert_eq!(
            read_own_watermark(&rt, MODEL)
                .await
                .expect("read active watermark"),
            Some(2)
        );

        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let retained = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM ann_write_log WHERE embedding_model = ?1".into(),
                params: vec![SqlValue::Text(MODEL.into())],
                label: Some("test_pathless_compacted_floor_empty_log".into()),
            })
            .await
            .expect("count retained tail");
        assert!(
            matches!(retained, Some(SqlValue::Integer(0))),
            "the first checkpoint must compact its retained log, got {retained:?}"
        );
        drop(reader);

        // Remove the ephemeral cache to force a full scan without appending
        // a log row; a generation-only bump now uses incremental maintenance.
        clear_key(&ann, &key).await;
        bump_generation(&ann, &key).await;
        let second = ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("cache-miss full checkpoint");
        assert!(
            matches!(second, AnnEnsureStatus::Built { vectors: 2 }),
            "the later full scan must remain publishable, got {second:?}"
        );
        assert!(is_current(&ann, &key).await);

        let (_hits, applied) = search_loaded_with_seq(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .expect("search installed bridge")
            .expect("bridge remains installed");
        assert_eq!(applied, 2, "the bridge must advertise the inherited floor");
        assert_eq!(
            read_own_watermark(&rt, MODEL)
                .await
                .expect("read retained active watermark"),
            Some(2)
        );
    }

    /// A recall in the pathless install-before-activation window must wait and revalidate, not evict the pending candidate.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn pathless_pending_reader_waits_for_checkpoint_activation() {
        const MODEL: &str = "pathless-pending-publication-wait";
        let rt = KhiveRuntime::memory().expect("runtime");
        provision_test_vector_store(&rt, MODEL, 4);
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        register_consumer(&rt, MODEL)
            .await
            .expect("register pending consumer");

        let candidate_id = Uuid::new_v4();
        let mut candidate = tiny_bridge(candidate_id, 1);
        candidate.set_applied_seq(2);
        assert!(install_replacing(&ann, &key, candidate).await);

        let publication_lock = model_warm_lock(&ann, &key).await;
        let publication_guard = publication_lock.lock().await;
        let waiting = ann.pathless_pending_publication_wait.notified();
        let task_rt = rt.clone();
        let task_ann = ann.clone();
        let task_key = key.clone();
        let reader = tokio::spawn(async move {
            fresh_tail_leg(
                &task_rt,
                &task_ann,
                &task_key,
                MODEL,
                &[1.0, 0.0, 0.0, 0.0],
                1,
                Some(2),
            )
            .await
        });
        waiting.await;
        assert!(
            !reader.is_finished(),
            "the pending reader must be blocked behind checkpoint publication"
        );

        raise_watermark_with_authority(&rt, MODEL, 2, WatermarkAuthority::PendingOrActive)
            .await
            .expect("activate checkpoint");
        drop(publication_guard);

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), reader)
            .await
            .expect("reader must resume after activation")
            .expect("reader task must not panic");
        assert!(matches!(outcome, FreshTailOutcome::Ops(_)));
        let installed = ann.indexes.read().await;
        let bridge = installed
            .get(&key)
            .expect("activation must preserve the installed candidate");
        assert_eq!(bridge.id_map, vec![candidate_id]);
    }

    /// If publication loses its registration while a reader waits, revalidation must still evict and return empty.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn pathless_pending_reader_evicts_after_registration_loss() {
        const MODEL: &str = "pathless-pending-publication-loss";
        let rt = KhiveRuntime::memory().expect("runtime");
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        register_consumer(&rt, MODEL)
            .await
            .expect("register pending consumer");

        let mut candidate = tiny_bridge(Uuid::new_v4(), 1);
        candidate.set_applied_seq(2);
        assert!(install_replacing(&ann, &key, candidate).await);

        let publication_lock = model_warm_lock(&ann, &key).await;
        let publication_guard = publication_lock.lock().await;
        let waiting = ann.pathless_pending_publication_wait.notified();
        let task_rt = rt.clone();
        let task_ann = ann.clone();
        let task_key = key.clone();
        let reader = tokio::spawn(async move {
            fresh_tail_leg(
                &task_rt,
                &task_ann,
                &task_key,
                MODEL,
                &[1.0, 0.0, 0.0, 0.0],
                1,
                Some(2),
            )
            .await
        });
        waiting.await;
        assert!(!reader.is_finished());

        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "DELETE FROM ann_consumer_watermark \
                      WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                    .into(),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(ANN_WILDCARD_NS.into()),
                    SqlValue::Text(MODEL.into()),
                ],
                label: Some("test_pathless_pending_registration_loss".into()),
            })
            .await
            .expect("simulate pending registration retirement");
        drop(writer);
        drop(publication_guard);

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), reader)
            .await
            .expect("reader must resume after publication ends")
            .expect("reader task must not panic");
        match outcome {
            FreshTailOutcome::Replace(hits, _) => assert!(hits.is_empty()),
            FreshTailOutcome::Ops(_) | FreshTailOutcome::Skipped(_) => {
                panic!("registration loss must replace captured candidates")
            }
        }
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "registration loss must evict the unprotected candidate"
        );
        assert_eq!(
            read_own_watermark(&rt, MODEL)
                .await
                .expect("read re-registration"),
            Some(PENDING_WATERMARK),
            "the returning consumer must re-register closed"
        );
    }

    /// A slower process must check the durable watermark under the segment lock before writing files, or it can overwrite a newer commit.
    #[tokio::test]
    async fn file_backed_stale_checkpoint_does_not_overwrite_newer_segment() {
        const MODEL: &str = "file-backed-stale-checkpoint";
        let rt = test_runtime_with_hash_embedder(MODEL, 4);
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        register_consumer(&rt, MODEL)
            .await
            .expect("register pending consumer");

        let winner_id = Uuid::new_v4();
        let mut winner = tiny_bridge(winner_id, 9);
        winner.set_applied_seq(9);
        assert!(
            checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                MODEL,
                winner,
                CheckpointPublication {
                    generation: 9,
                    epoch: 0,
                    authority: WatermarkAuthority::PendingOrActive,
                },
            )
            .await,
            "the first checkpoint must activate the pending registration"
        );

        let mut stale = tiny_bridge(Uuid::new_v4(), 3);
        stale.set_applied_seq(3);
        assert!(
            !checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                MODEL,
                stale,
                CheckpointPublication {
                    generation: 3,
                    epoch: 0,
                    authority: WatermarkAuthority::Active,
                },
            )
            .await,
            "a checkpoint behind the durable watermark must lose before persistence"
        );

        let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
        let commit = read_commit_info(&dir)
            .expect("read commit")
            .expect("persisted commit");
        assert_eq!(commit.last_applied_seq, Some(9));
        let persisted = AnnBridge::load(&dir).expect("load winner segment");
        assert_eq!(persisted.id_map, vec![winner_id]);
        assert_eq!(
            read_own_watermark(&rt, MODEL)
                .await
                .expect("read durable watermark"),
            Some(9)
        );
    }

    /// A failed replacement persist must not discard the still-protected incumbent while the retained tail remains.
    #[tokio::test]
    async fn file_backed_persist_failure_preserves_active_incumbent() {
        const MODEL: &str = "file-backed-persist-failure-fallback";
        let rt = test_runtime_with_hash_embedder(MODEL, 4);
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        register_consumer(&rt, MODEL)
            .await
            .expect("register pending consumer");
        raise_watermark_with_authority(&rt, MODEL, 1, WatermarkAuthority::PendingOrActive)
            .await
            .expect("activate incumbent checkpoint");

        let incumbent_id = Uuid::new_v4();
        let mut incumbent = tiny_bridge(incumbent_id, 1);
        incumbent.set_applied_seq(1);
        assert!(install_replacing(&ann, &key, incumbent).await);

        let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
        std::fs::create_dir_all(&dir).expect("create segment directory");
        std::fs::create_dir(dir.join("metadata.bin"))
            .expect("block metadata file publication with a directory");

        let mut replacement = tiny_bridge(Uuid::new_v4(), 2);
        replacement.set_applied_seq(2);
        assert!(
            !checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                MODEL,
                replacement,
                CheckpointPublication {
                    generation: 2,
                    epoch: 0,
                    authority: WatermarkAuthority::Active,
                },
            )
            .await,
            "the deliberately blocked persist must fail publication"
        );

        let installed = ann.indexes.read().await;
        let bridge = installed
            .get(&key)
            .expect("active incumbent must survive replacement persistence failure");
        assert_eq!(bridge.generation, 1);
        assert_eq!(bridge.id_map, vec![incumbent_id]);
    }

    /// A strictly newer candidate replaces the installed older generation.
    #[tokio::test]
    async fn install_replacing_replaces_older_installed_entry() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");
        let older_id = Uuid::new_v4();
        let newer_id = Uuid::new_v4();

        install_replacing(&ann, &key, tiny_bridge(older_id, 1)).await;
        install_replacing(&ann, &key, tiny_bridge(newer_id, 9)).await;

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("an entry must be installed");
        assert_eq!(bridge.generation, 9);
        assert_eq!(bridge.id_map, vec![newer_id]);
    }

    /// Equal generations replace: under the single-flight model lock a tie is an ordered later step of the same warm task.
    #[tokio::test]
    async fn install_replacing_replaces_on_equal_generation() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();

        install_replacing(&ann, &key, tiny_bridge(first_id, 3)).await;
        install_replacing(&ann, &key, tiny_bridge(second_id, 3)).await;

        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("an entry must be installed");
        assert_eq!(
            bridge.id_map,
            vec![second_id],
            "on an equal generation, the later ordered install must replace"
        );
    }

    /// An installed generation behind the write counter is not current.
    #[tokio::test]
    async fn is_current_false_when_installed_generation_behind_counter() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");

        // Install a bridge stamped with generation 1 (as if built before any
        // write bumped the counter further).
        install_replacing(&ann, &key, tiny_bridge(Uuid::new_v4(), 1)).await;
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
        install_replacing(&ann, &key, tiny_bridge(Uuid::new_v4(), 2)).await;
        assert!(
            is_current(&ann, &key).await,
            "installed generation (2) now matches the write-generation counter (2)"
        );
    }

    /// `is_current` on an absent key is false, a genuine cache miss that falls through to the ensure/build path.
    #[tokio::test]
    async fn is_current_false_when_absent() {
        let ann = new_shared();
        let key = AnnKey::new("model-x");
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
                .contains_key(&AnnKey::from_token(MODEL)),
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

    #[test]
    fn cancelled_store_join_emits_phase_cancelled() {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("single-worker runtime");
        executor.block_on(async {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).expect("worker started");
                release_rx.recv().expect("release blocker");
            });
            started_rx.await.expect("blocking slot is occupied");
            let queued = tokio::task::spawn_blocking(|| Ok(AnnEnsureStatus::EmptyCorpus));
            queued.abort();
            release_tx.send(()).expect("release blocking slot");
            blocker.await.expect("blocker joined");
            let result = crate::store_access::join_store_task("memory.ann.vector_store", queued).await;

            let rt = KhiveRuntime::memory().expect("in-memory runtime");
            let token = rt.authorize(Namespace::local()).expect("authorize local");
            emit_ann_warm_terminal_phase(&rt, &token, "cancelled-store-join", &result, 1, None, AnnWarmDetails::default()).await;
            let page = rt.events(&token).expect("event store").query_events(
                khive_storage::EventFilter::default(),
                khive_storage::types::PageRequest { limit: 10, offset: 0 },
            ).await.expect("terminal events");
            assert_eq!(page.items.len(), 1, "exactly one terminal event: {page:?}");
            assert_eq!(page.items[0].kind, khive_types::EventKind::PhaseCancelled,
                "a cancelled acquisition join must emit PhaseCancelled, not PhaseCompleted: {result:?}");
        });
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

    /// Provision the real sqlite-vec store that backs a manually installed
    /// test bridge. Production bridges are built from an existing store; tests
    /// that install one directly must preserve that schema invariant.
    fn provision_test_vector_store(rt: &KhiveRuntime, model: &str, dims: usize) {
        rt.register_embedder(HashVecProvider {
            model_name: model.to_owned(),
            dims,
        });
        let token = rt
            .authorize(Namespace::local())
            .expect("authorize vector-store fixture");
        drop(
            rt.vectors_for_model(&token, model)
                .expect("provision vector-store fixture"),
        );
    }

    fn test_runtime_with_hash_embedder(model: &str, dims: usize) -> TestRuntime {
        let tmp = tempfile::Builder::new()
            .prefix("khive-memory-ann-test-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp db dir");
        let db_path = tmp.path().join("khive-graph.db");
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
        TestRuntime {
            runtime: rt,
            _temp_dir: tmp,
        }
    }

    #[tokio::test]
    async fn session_exact_snapshot_proves_receipt_with_candidates_and_zero_hits() {
        const MODEL: &str = "session-exact-proof-model";
        const CONTENT: &str = "distinctive session exact proof memory";
        let rt = test_runtime_with_hash_embedder(MODEL, 8);
        let token = rt.authorize(Namespace::local()).expect("local token");
        let (note, fences) = rt
            .create_note_with_decay_for_embedding_model_with_visibility(
                &token,
                "memory",
                None,
                CONTENT,
                Some(0.8),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("write note and exact visibility receipt");
        let seq = fences
            .iter()
            .find(|(model, _)| model == MODEL)
            .map(|(_, seq)| *seq)
            .expect("model fence");
        let query = fnv_to_vec(CONTENT, 8);
        let candidates =
            session_exact_candidates(&rt, MODEL, &query, &["local".into()], "local", seq, 10)
                .await
                .expect("one-statement exact read")
                .expect("matching receipt is proven");
        assert!(
            candidates
                .iter()
                .any(|(id, score)| *id == note.id && *score > 0.95),
            "candidate-producing read must include the recent matching note: {candidates:?}"
        );

        let empty = session_exact_candidates(&rt, MODEL, &query, &[], "local", seq, 10)
            .await
            .expect("empty-visible-set read")
            .expect("the same statement still proves the receipt");
        assert!(empty.is_empty(), "proof must survive zero candidates");
        let zero_limit =
            session_exact_candidates(&rt, MODEL, &query, &["local".into()], "local", seq, 0)
                .await
                .expect("zero-limit read")
                .expect("zero limit still proves the receipt");
        assert!(zero_limit.is_empty());
    }

    #[tokio::test]
    async fn session_exact_snapshot_refuses_wrong_namespace_model_and_future_fence() {
        const MODEL: &str = "session-exact-wrong-receipt-model";
        let rt = test_runtime_with_hash_embedder(MODEL, 8);
        let token = rt.authorize(Namespace::local()).expect("local token");
        let (_note, fences) = rt
            .create_note_with_decay_for_embedding_model_with_visibility(
                &token,
                "memory",
                None,
                "session exact wrong receipt",
                Some(0.8),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("write note and exact visibility receipt");
        let seq = fences[0].1;
        let query = fnv_to_vec("session exact wrong receipt", 8);
        let visible = ["local".into()];
        assert!(
            session_exact_candidates(&rt, MODEL, &query, &visible, "other", seq, 10)
                .await
                .expect("namespace mismatch is not a read error")
                .is_none()
        );
        assert!(
            session_exact_candidates(&rt, MODEL, &query, &visible, "local", seq + 1, 10)
                .await
                .expect("future fence is not a read error")
                .is_none()
        );

        let mut writer = rt.sql().writer().await.expect("SQL writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE ann_write_log SET op = 'delete' WHERE seq = ?1".into(),
                params: vec![SqlValue::Integer(seq as i64)],
                label: Some("session_exact_wrong_op_fixture".into()),
            })
            .await
            .expect("alter fixture receipt operation");
        drop(writer);
        assert!(
            session_exact_candidates(&rt, MODEL, &query, &visible, "local", seq, 10)
                .await
                .expect("delete receipt is not a read error")
                .is_none()
        );

        let mut writer = rt.sql().writer().await.expect("SQL writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE ann_write_log SET op = 'upsert', embedding_model = 'other-model' WHERE seq = ?1"
                    .into(),
                params: vec![SqlValue::Integer(seq as i64)],
                label: Some("session_exact_wrong_model_fixture".into()),
            })
            .await
            .expect("alter fixture receipt model");
        drop(writer);
        assert!(
            session_exact_candidates(&rt, MODEL, &query, &visible, "local", seq, 10)
                .await
                .expect("model mismatch is not a read error")
                .is_none()
        );
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
        let key = AnnKey::from_token(MODEL);

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

    // ── ADR-079 Amendment 1: write-log restart classification ──────────────

    /// A same-cardinality replacement must classify Stale-tail and replay, never trust the segment Hot.
    #[tokio::test]
    async fn ensure_ann_for_model_restart_same_cardinality_replacement_replays_tail() {
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
            matches!(
                status,
                AnnEnsureStatus::LoadedSnapshot | AnnEnsureStatus::Built { .. }
            ),
            "a same-cardinality corpus content change must be detected via the \
             write-log tail (Stale-tail replay, or Stale-rebuild when the tail \
             exceeds the threshold) rather than silently classifying Hot, got: {status:?}"
        );
        // The replacement note's write left a tail row, so a Hot adoption of
        // the pre-change segment (which would report LoadedSnapshot WITHOUT
        // containing the replacement) is ruled out by searching for it.
        let key = AnnKey::from_token(MODEL);
        let query = fnv_to_vec("restart signal note REPLACEMENT", DIMS);
        let hits = search_loaded(&ann2, &key, &query, 5)
            .await
            .expect("search must succeed")
            .expect("index must be installed");
        assert!(
            hits.iter().any(|(_, score)| *score > 0.99),
            "the replayed index must contain the replacement note's vector, got: {hits:?}"
        );
    }

    /// A vector-only re-embed's log row (ADR-107) must make a restart replay the new bytes, not classify Hot on stale ones.
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
            // The write-path contract requires every vector mutation to append
            // a log row; a reindexer that bypassed it would classify Hot on
            // stale bytes at the next restart.
            w.execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      SELECT n.namespace, ?2, 'note', 'note.content', ?1, 'upsert' \
                      FROM notes n WHERE n.id = ?1"
                    .into(),
                params: vec![
                    SqlValue::Text(note_ids[0].to_string()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test_vector_only_reindex_log".into()),
            })
            .await
            .expect("append reindex log row");
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
            matches!(
                status,
                AnnEnsureStatus::LoadedSnapshot | AnnEnsureStatus::Built { .. }
            ),
            "a logged vector-only re-embed must classify as Stale (tail replay \
             or rebuild), never Hot on the pre-reindex bytes, got: {status:?}"
        );
        // The replayed index must serve the NEW embedding for the re-embedded
        // note — a Hot adoption of the stale segment would miss it.
        let key = AnnKey::from_token(MODEL);
        let replacement: Vec<f32> = (0..DIMS).map(|i| (i as f32 + 100.0) / 7.0).collect();
        let hits = search_loaded(&ann2, &key, &replacement, 1)
            .await
            .expect("search must succeed")
            .expect("index must be installed");
        assert_eq!(
            hits.first().map(|(id, _)| *id),
            Some(note_ids[0]),
            "the re-embedded note must be nearest to its new vector, got: {hits:?}"
        );
        assert!(
            hits[0].1 > 0.99,
            "the served vector must be the re-embedded bytes, got score {}",
            hits[0].1
        );
    }

    /// A final tail upsert whose note fails the join predicate is not a contradiction; replay tombstones it instead of going Cold.
    #[tokio::test]
    async fn restart_tail_upsert_for_soft_deleted_note_replays_as_delete() {
        const MODEL: &str = "ann-warm-restart-join-predicate-model";
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
                    &format!("join predicate note {i}"),
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

        // A re-embed logs its upsert, then the note is soft-deleted by a path
        // that never cleans the vector row: the vec row and the final upsert
        // both survive while the join predicate now excludes the note.
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      SELECT n.namespace, ?2, 'note', 'note.content', ?1, 'upsert' \
                      FROM notes n WHERE n.id = ?1"
                    .into(),
                params: vec![
                    SqlValue::Text(note_ids[0].to_string()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test_join_predicate_log".into()),
            })
            .await
            .expect("append upsert log row");
            w.execute(SqlStatement {
                sql: "UPDATE notes SET deleted_at = created_at WHERE id = ?1".into(),
                params: vec![SqlValue::Text(note_ids[0].to_string())],
                label: Some("test_join_predicate_soft_delete".into()),
            })
            .await
            .expect("soft-delete note row without vector cleanup");
        }

        // Restart: live = 3, tail = 1 ≤ ceil(0.20 × 3) → Stale-tail replay.
        let ann2 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
            .await
            .expect("post-restart warm");
        assert!(
            matches!(status, AnnEnsureStatus::LoadedSnapshot),
            "a predicate-failing final upsert must replay as a delete within \
             Stale-tail adoption, not force a Cold rebuild, got: {status:?}"
        );
        let key = AnnKey::from_token(MODEL);
        let query = fnv_to_vec("join predicate note 0", DIMS);
        let hits = search_loaded(&ann2, &key, &query, 4)
            .await
            .expect("search must succeed")
            .expect("index must be installed");
        assert!(
            !hits
                .iter()
                .any(|(id, score)| *id == note_ids[0] && *score > 0.99),
            "the soft-deleted note must be tombstoned by the replayed delete, got: {hits:?}"
        );
    }

    /// A final tail upsert with no vector row at all contradicts the committed log, so replay must fall through to a Cold rebuild.
    #[tokio::test]
    async fn restart_tail_upsert_with_absent_vector_row_goes_cold() {
        const MODEL: &str = "ann-warm-restart-contradiction-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..4u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("contradiction note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        }

        let ann1 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
            .await
            .expect("first warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "expected a fresh build over 4 vectors, got: {status:?}"
        );

        // A committed final upsert for a subject with no vector row anywhere —
        // impossible under the same-transaction write contract, so it can only
        // mean corruption. Replay must not fabricate or skip it.
        {
            let phantom = Uuid::new_v4();
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES ('local', ?2, 'note', 'note.content', ?1, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Text(phantom.to_string()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test_contradiction_log".into()),
            })
            .await
            .expect("append phantom upsert log row");
        }

        // Restart: live = 4, tail = 1 ≤ ceil(0.20 × 4) → Stale-tail is
        // attempted, the point read finds no vector row, replay errs, and the
        // classifier falls through Cold to a full rebuild.
        let ann2 = new_shared();
        let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
            .await
            .expect("post-restart warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "a log/corpus contradiction must force a Cold rebuild, never a \
             segment adoption, got: {status:?}"
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
        let key = AnnKey::from_token(MODEL);
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
            // Contract-mandated log append for the vector overwrite (ADR-107
            // supersession note) — the classifier replays this row after the
            // epoch bump invalidates the warm cache.
            w.execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      SELECT n.namespace, ?2, 'note', 'note.content', ?1, 'upsert' \
                      FROM notes n WHERE n.id = ?1"
                    .into(),
                params: vec![
                    SqlValue::Text(note_ids[0].to_string()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test_durable_epoch_vector_reindex_log".into()),
            })
            .await
            .expect("append reindex log row");
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
            matches!(
                status,
                AnnEnsureStatus::LoadedSnapshot | AnnEnsureStatus::Built { .. }
            ),
            "the warm daemon must re-adopt (tail replay or rebuild) once its \
             durable-epoch check detects the out-of-process reindex, got: {status:?}"
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
        let key = AnnKey::from_token(MODEL);

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

    // ── ADR-118: fresh-tail exact leg ───────────────────────────────────────

    /// The no-index fallback follows ADR-118's ceil(threshold × live corpus) ceiling, not a flat row cap (#1161).
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_no_index_cap_tracks_live_corpus_fraction() {
        const MODEL: &str = "adr118-corpus-relative-cap-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let mut ids = Vec::new();

        for i in 0..6u32 {
            let note = rt
                .create_note_with_decay_for_embedding_model(
                    &token,
                    "memory",
                    None,
                    &format!("corpus-relative cap note {i}"),
                    Some(0.7),
                    0.01,
                    None,
                    vec![],
                    None,
                )
                .await
                .expect("create note");
            ids.push(note.id);
        }

        let ops = match fresh_tail_capped_at_threshold(&rt, MODEL, 0.20).await {
            FreshTailOutcome::Ops(ops) => ops,
            FreshTailOutcome::Replace(..) => {
                panic!("no-index capped leg must not produce replacement candidates")
            }
            FreshTailOutcome::Skipped(reason) => {
                panic!("no-index capped leg unexpectedly skipped: {reason}")
            }
        };
        let selected: Vec<Uuid> = ops.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            selected,
            ids[4..].to_vec(),
            "ceil(0.20 × 6) must select the two newest raw log rows"
        );
    }

    /// A skip that discards an error must carry it out to the caller: the
    /// label alone cannot say whether the store lost its write log or was
    /// merely busy, and retry-vs-rebuild turns on that difference. The
    /// control arm is the same call before the fault, which must not skip —
    /// otherwise this passes on a leg that sits out unconditionally.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_capped_leg_carries_its_read_error_into_the_skip_reason() {
        const MODEL: &str = "adr118-skip-reason-carries-error-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("skip reason fixture note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        }

        match fresh_tail_capped_at_threshold(&rt, MODEL, 1.0).await {
            FreshTailOutcome::Ops(ops) => assert!(
                !ops.is_empty(),
                "control: a healthy store must replay the seeded tail"
            ),
            FreshTailOutcome::Replace(..) => {
                panic!("the no-index capped leg never replaces candidates")
            }
            FreshTailOutcome::Skipped(reason) => {
                panic!("control: a healthy store must not skip, got: {reason}")
            }
        }

        // Fault injection: remove the write-log table the leg's existence
        // probe reads, so its next call fails for a nameable reason. (Test
        // fixture database, mirroring the DROP TABLE fault pattern already
        // used in this module and in handlers/recall.rs.)
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("fault injection writer");
            w.execute(SqlStatement {
                sql: "DROP TABLE ann_write_log".into(),
                params: vec![],
                label: Some("test_drop_write_log_for_skip_reason".into()),
            })
            .await
            .expect("drop the write-log table");
        }

        match fresh_tail_capped_at_threshold(&rt, MODEL, 1.0).await {
            FreshTailOutcome::Skipped(reason) => {
                assert_eq!(
                    reason.label(),
                    "fresh-tail: tail-existence read failed",
                    "the failure-site label must be unchanged by the enrichment"
                );
                let detail = reason
                    .detail()
                    .expect("a skip constructed while holding an error must carry it");
                assert!(
                    detail.contains("ann_write_log"),
                    "the carried error must name what actually failed, got: {detail:?}"
                );
                let rendered = reason.to_string();
                assert!(
                    rendered.starts_with(reason.label()) && rendered.contains(detail),
                    "the served reason must carry the label and the error, got: {rendered:?}"
                );
            }
            FreshTailOutcome::Ops(_) | FreshTailOutcome::Replace(..) => {
                panic!("a missing write-log table must make the capped leg sit out")
            }
        }
    }

    /// The policy-disabled arm holds no error, and must keep emitting exactly
    /// the bare label it always did — through the real leg, not only through
    /// a hand-built value. The control arm is the same fixture with the leg
    /// enabled, which must not skip at all.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_disabled_by_policy_skips_with_a_bare_label() {
        const MODEL: &str = "adr118-disabled-policy-bare-label-test-model";
        const DIMS: usize = 8;

        async fn run_leg(fresh_tail_enabled: bool) -> FreshTailOutcome {
            let rt = KhiveRuntime::memory()
                .expect("in-memory runtime")
                .with_ann_fresh_tail_enabled(fresh_tail_enabled);
            provision_test_vector_store(&rt, MODEL, DIMS);
            register_consumer(&rt, MODEL)
                .await
                .expect("register this consumer");
            raise_watermark_with_authority(&rt, MODEL, 0, WatermarkAuthority::PendingOrActive)
                .await
                .expect("activate this consumer");
            let ann = new_shared();
            let key = AnnKey::new(MODEL);
            fresh_tail_leg(&rt, &ann, &key, MODEL, &[0.0_f32; DIMS], 10, Some(0)).await
        }

        match run_leg(false).await {
            FreshTailOutcome::Skipped(reason) => {
                assert!(
                    reason
                        .label()
                        .starts_with("fresh-tail leg disabled by runtime policy"),
                    "got: {}",
                    reason.label()
                );
                assert_eq!(
                    reason.detail(),
                    None,
                    "this site holds no error, so it must carry none"
                );
                assert_eq!(
                    reason.to_string(),
                    reason.label(),
                    "an error-free skip must render as the bare label, with no \
                     separator and no placeholder"
                );
            }
            FreshTailOutcome::Ops(_) | FreshTailOutcome::Replace(..) => {
                panic!("a leg disabled by runtime policy must sit the query out")
            }
        }

        match run_leg(true).await {
            FreshTailOutcome::Ops(_) => {}
            FreshTailOutcome::Replace(..) => {
                panic!("control: an enabled leg over an empty log must not replace")
            }
            FreshTailOutcome::Skipped(reason) => {
                panic!("control: an enabled leg must not skip, got: {reason}")
            }
        }
    }

    /// A subject in the stale warm index whose final tail op is delete must be dropped from the merged list (#1828).
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_subject_whose_final_tail_op_is_delete() {
        const MODEL: &str = "adr118-tail-delete-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let target = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "tail delete target note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create target note");
        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("tail delete filler note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create filler note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        let status = ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "expected initial build of 4 vectors, got: {status:?}"
        );

        // Delete AFTER the bridge is warm — a write only bumps generation, it
        // never evicts the served bridge, so its cached graph still nominates
        // the now-deleted subject.
        rt.delete_note(&token, target.id, false)
            .await
            .expect("soft delete target");

        let query = fnv_to_vec("tail delete target note", DIMS);
        let raw = search_loaded(&ann, &key, &query, 10)
            .await
            .expect("search")
            .expect("bridge still warm");
        assert!(
            raw.iter().any(|(id, _)| *id == target.id),
            "sanity: the stale warm bridge must still nominate the deleted \
             subject from its cached graph before the fresh-tail leg runs"
        );

        let s = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark");
        let ops = match fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await {
            FreshTailOutcome::Ops(ops) => ops,
            FreshTailOutcome::Replace(..) => panic!("fresh-tail leg unexpectedly re-resolved"),
            FreshTailOutcome::Skipped(reason) => {
                panic!("fresh-tail leg unexpectedly skipped: {reason}")
            }
        };
        let merged = merge_fresh_tail(raw, &query, ops);
        assert!(
            !merged.iter().any(|(id, _)| *id == target.id),
            "a subject whose final tail op is delete must be dropped from the \
             merged candidate list even though the stale ANN index still \
             nominates it, got: {merged:?}"
        );
    }

    /// A subject in both the stale candidates and the tail must appear once, carrying the tail's exact score.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_dedups_with_tail_winning() {
        const MODEL: &str = "adr118-tail-dedup-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let target = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "tail dedup original content",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create target note");
        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("tail dedup filler note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create filler note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        let status = ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        assert!(
            matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
            "expected initial build of 4 vectors, got: {status:?}"
        );

        const UPDATED_TEXT: &str = "tail dedup UPDATED content, unrelated to the original";
        rt.update_note_with_embedding_report(
            &token,
            target.id,
            khive_runtime::NotePatch::new(None, Some(UPDATED_TEXT.to_string()), None, None, None),
        )
        .await
        .map(|(row, _report)| row)
        .expect("update target note");

        // Query the segment's stale embedding of the ORIGINAL content: the
        // stale ANN index nominates the subject at its old (now-superseded)
        // score, while the tail carries the exact score against the updated
        // embedding — they must collapse to one entry, tail winning.
        let query = fnv_to_vec("tail dedup original content", DIMS);
        let raw = search_loaded(&ann, &key, &query, 10)
            .await
            .expect("search")
            .expect("bridge still warm");
        let stale_score = raw
            .iter()
            .find(|(id, _)| *id == target.id)
            .map(|(_, score)| *score)
            .expect("sanity: stale ANN index must still nominate the pre-update subject");

        let s = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark");
        let ops = match fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await {
            FreshTailOutcome::Ops(ops) => ops,
            FreshTailOutcome::Replace(..) => panic!("fresh-tail leg unexpectedly re-resolved"),
            FreshTailOutcome::Skipped(_) => panic!("fresh-tail leg unexpectedly skipped"),
        };
        let merged = merge_fresh_tail(raw, &query, ops);

        let matches: Vec<&(Uuid, f32)> = merged.iter().filter(|(id, _)| *id == target.id).collect();
        assert_eq!(
            matches.len(),
            1,
            "the subject must appear exactly once in the merged list, got: {merged:?}"
        );
        let exact_score = exact_cosine(&query, &fnv_to_vec(UPDATED_TEXT, DIMS));
        assert!(
            (matches[0].1 - exact_score).abs() < 1e-6,
            "the merged entry must carry the tail's exact score ({exact_score}) \
             rather than the stale segment's score ({stale_score}), got {}",
            matches[0].1
        );
    }

    /// A pathless recall racing a checkpoint must re-search the installed replacement, not merge a floored tail into stale candidates.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_pathless_mismatch_replaces_pre_checkpoint_candidates() {
        const MODEL: &str = "adr118-pathless-reresolve-model";
        const DIMS: usize = 8;
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("pathless baseline note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create baseline note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("initial warm");

        const FRESH_TEXT: &str = "pathless checkpoint distinctive fresh note";
        let fresh = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                FRESH_TEXT,
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create post-checkpoint note");
        bump_generation(&ann, &key).await;

        let query = fnv_to_vec(FRESH_TEXT, DIMS);
        let (captured, s1) = search_loaded_with_seq(&ann, &key, &query, 10)
            .await
            .expect("search old bridge")
            .expect("old bridge installed");
        assert!(
            captured.iter().all(|(id, _)| *id != fresh.id),
            "the pre-checkpoint bridge must not contain the fresh note"
        );

        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("publish replacement");
        let s2 = bridge_applied_seq(&ann, &key)
            .await
            .expect("replacement watermark");
        assert!(s2 > s1, "replacement must advance the applied watermark");
        assert!(
            !tail_exists(&rt, MODEL, s1)
                .await
                .expect("read compacted tail"),
            "checkpoint compaction must remove the old bridge's intervening tail"
        );

        let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
        let replacement = match outcome {
            FreshTailOutcome::Replace(hits, _) => hits,
            _ => panic!("pathless mismatch must replace stale candidates"),
        };
        assert!(
            replacement.iter().any(|(id, _)| *id == fresh.id),
            "re-resolution must recover the fresh note from the installed replacement"
        );
    }

    /// A negative (pending/recovering) registry minimum must not wrap to `u64::MAX` and suppress a real, fully-retained tail.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_negative_peer_minimum_keeps_real_tail_visible() {
        const MODEL: &str = "adr118-negative-minimum-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("negative minimum baseline note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create baseline note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark");
        let fresh = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "negative minimum distinctive fresh note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create tail note");
        let sql = rt.sql();
        ann_registry::register_pending(sql.as_ref(), "pending-peer", ANN_WILDCARD_NS, MODEL)
            .await
            .expect("register pending peer");

        let query = fnv_to_vec("negative minimum distinctive fresh note", DIMS);
        let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await;
        let ops = match outcome {
            FreshTailOutcome::Ops(ops) => ops,
            _ => panic!("negative minimum must preserve the ordinary tail scan"),
        };
        assert!(
            ops.iter()
                .any(|(id, embedding)| *id == fresh.id && embedding.is_some()),
            "the fully retained tail must include the fresh note"
        );
    }

    /// A pending peer's negative minimum is coherent with the installed
    /// pathless bridge; re-resolution must retain its candidates and tail.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_pathless_negative_peer_minimum_merges_candidates_and_tail() {
        const MODEL: &str = "adr118-pathless-negative-minimum-model";
        const DIMS: usize = 8;
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("pathless negative minimum baseline note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create baseline note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm pathless bridge");
        const FRESH_TEXT: &str = "pathless negative minimum distinctive fresh note";
        let fresh = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                FRESH_TEXT,
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create tail note");
        ann_registry::register_pending(rt.sql().as_ref(), "pending-peer", ANN_WILDCARD_NS, MODEL)
            .await
            .expect("register pending peer");

        let query = fnv_to_vec(FRESH_TEXT, DIMS);
        let (candidates, _) = search_loaded_with_seq(&ann, &key, &query, 10)
            .await
            .expect("search installed bridge")
            .expect("pathless bridge installed");
        assert!(!candidates.is_empty(), "bridge must supply candidates");
        assert!(
            candidates.iter().all(|(id, _)| *id != fresh.id),
            "fresh note must exist only in the tail"
        );

        let outcome = fresh_tail_pathless_reresolve(
            &rt,
            &ann,
            &key,
            MODEL,
            FreshTailSearch::new(&query, 10, AnnScoreRoute::Memory),
            None,
        )
        .await;
        let merged = match outcome {
            FreshTailOutcome::Replace(hits, _) => hits,
            _ => panic!("pathless re-resolution must replace stale candidates"),
        };
        assert!(
            candidates
                .iter()
                .all(|(id, _)| merged.iter().any(|(served, _)| served == id)),
            "negative minimum must retain re-resolved bridge candidates: {merged:?}"
        );
        assert!(
            merged.iter().any(|(id, _)| *id == fresh.id),
            "negative minimum must merge the final fresh tail: {merged:?}"
        );
    }

    /// An active registry ahead of the only persisted base cannot establish coverage of compacted writes.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_registry_minimum_exceeds_persisted_base() {
        const MODEL: &str = "adr118-compaction-guard-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("compaction guard seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");

        // A write lands after the checkpoint: the log advances, but the
        // served bridge is never re-persisted (only `ensure_ann_for_model`
        // does that), so its own watermark stays pinned at `s1`.
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "compaction guard post-checkpoint write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create post-checkpoint note");

        // Simulate a peer process's checkpoint: raise the shared durable
        // registry watermark past `s1` and compact the log through it —
        // exactly `checkpoint_raise_compact_readopt`'s raise+compact steps,
        // without the persist/re-adopt this bridge never observes.
        let (_live, tail_before) = scope_counts(&rt, MODEL, s1)
            .await
            .expect("scope counts before compaction");
        assert!(tail_before > 0, "sanity: a tail must exist above s1");
        let s2 = s1 + tail_before;
        raise_watermark(&rt, MODEL, s2)
            .await
            .expect("raise registry watermark past bridge watermark");
        compact_log(&rt, MODEL).await.expect("compact log");

        let generation_before = current_generation(&ann, &key).await;
        let query = fnv_to_vec("compaction guard seed note 0", DIMS);
        let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
        match outcome {
            FreshTailOutcome::Replace(candidates, reason) => {
                assert!(candidates.is_empty());
                assert_eq!(
                    reason,
                    Some("fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates"),
                );
            }
            FreshTailOutcome::Ops(ops) => {
                panic!("a tail above the registry minimum cannot repair stale candidates: {ops:?}")
            }
            FreshTailOutcome::Skipped(_) => {
                panic!("an unproved publication must drop stale candidates, not skip")
            }
        }
        assert!(
            current_generation(&ann, &key).await > generation_before,
            "the mismatch must force re-adoption (bump_generation) so a \
             future query gets a fresh bridge"
        );
    }

    /// A peer's newer persisted segment must be loaded, searched directly, and returned as `Replace`, never merged with stale candidates.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_reresolves_to_a_newer_persisted_segment_on_mismatch() {
        const MODEL: &str = "adr118-reresolve-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("reresolve seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");

        // A new note lands after the checkpoint.
        let fresh = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "reresolve distinctive fresh note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create fresh note");

        let (_live, tail_before) = scope_counts(&rt, MODEL, s1)
            .await
            .expect("scope counts before peer checkpoint");
        assert!(tail_before > 0, "sanity: a tail must exist above s1");
        let s2 = s1 + tail_before;

        // Simulate a PEER PROCESS's real checkpoint: replay the tail into a
        // fresh load of the persisted segment, persist it back to disk at
        // s2, and raise+compact the registry — all WITHOUT touching this
        // process's in-memory `ann` map, which stays pinned at the stale s1
        // bridge (the mismatch this leg must detect and recover from).
        let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
        let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
        let (ops, new_s) = fetch_final_tail(&rt, MODEL, s1, None)
            .await
            .expect("fetch tail for peer replay");
        peer_bridge
            .apply_final_ops(ops, new_s)
            .expect("apply peer replay");
        peer_bridge
            .save_atomic(&dir)
            .expect("persist peer checkpoint");
        raise_watermark(&rt, MODEL, s2)
            .await
            .expect("raise registry watermark");
        compact_log(&rt, MODEL).await.expect("compact log");

        let generation_before = current_generation(&ann, &key).await;
        let query = fnv_to_vec("reresolve distinctive fresh note", DIMS);
        let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
        let candidates = match outcome {
            FreshTailOutcome::Replace(candidates, _) => candidates,
            FreshTailOutcome::Ops(_) => panic!(
                "expected re-resolution to replace candidates outright, not \
                 just return ops to merge into the stale bridge's candidates"
            ),
            FreshTailOutcome::Skipped(_) => {
                panic!("expected successful re-resolution, not a skip")
            }
        };
        assert!(
            candidates.iter().any(|(id, _)| *id == fresh.id),
            "the re-resolved segment's own search must surface the note \
             that only the peer's checkpoint (not this process's stale \
             bridge) reflects, got: {candidates:?}"
        );
        assert!(
            current_generation(&ann, &key).await > generation_before,
            "re-resolution must still force re-adoption so a future query \
             installs this segment as the served bridge"
        );
    }

    /// A delta HEAD can be valid while a chunk it names is gone. Its watermark
    /// then promises a re-resolution the segment load cannot deliver, and a
    /// write compacted into that chunk is in neither the stale candidates nor
    /// the retained log. The leg must drop the stale candidates with a
    /// disclosed reason, never skip and serve them, and never floor them.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_a_valid_delta_head_names_a_missing_chunk() {
        const MODEL: &str = "adr118-reresolve-broken-delta-chain-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("broken delta chain seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");

        let inside = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "broken delta chain note inside the published delta",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create delta note");

        // A peer publishes a delta checkpoint over the persisted base, then
        // raises the registry to the delta watermark and compacts through it,
        // leaving this process's in-memory bridge pinned at `s1`.
        let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
        let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
        let (ops, delta_s) = fetch_final_tail(&rt, MODEL, s1, None)
            .await
            .expect("fetch tail for peer replay");
        assert!(
            delta_s > s1,
            "sanity: the delta must cover a write above s1"
        );
        let raw_count = ops.len() as u64;
        peer_bridge
            .apply_final_ops(ops.clone(), delta_s)
            .expect("apply peer replay");
        peer_bridge.record_delta_batch(ops, delta_s, raw_count);
        assert!(
            !peer_bridge.needs_full_compaction(),
            "sanity: the peer checkpoint must publish a delta, not a full segment"
        );
        let publication = delta::write(&dir, &peer_bridge).expect("publish peer delta");
        raise_watermark(&rt, MODEL, delta_s)
            .await
            .expect("raise registry watermark to the delta watermark");
        compact_log(&rt, MODEL).await.expect("compact log");
        assert!(
            AnnBridge::load(&dir).is_ok(),
            "control: the intact chain must load before its chunk is removed"
        );

        std::fs::remove_file(dir.join(format!("memory_delta-{}.bin", publication.last_nonce)))
            .expect("remove the chunk the delta HEAD names");
        let base_seq = read_commit_info(&dir)
            .expect("read base commit")
            .and_then(|info| info.last_applied_seq)
            .expect("base watermark");
        assert_eq!(
            effective_persisted_state(&dir, base_seq)
                .expect("the delta HEAD alone is still valid")
                .0,
            delta_s,
            "precondition: the HEAD still promises the delta watermark, so the \
             mismatch preflight chooses re-resolution"
        );
        assert!(
            AnnBridge::load(&dir).is_err(),
            "precondition: the segment load must reject the broken chain"
        );
        let (retained, _) = fetch_final_tail(&rt, MODEL, s1, None)
            .await
            .expect("fetch the retained log above the bridge watermark");
        assert!(
            !retained.iter().any(|(id, _)| *id == inside.id),
            "precondition: the write inside the broken chain is gone from the \
             retained log, so no tail can restore it, got: {retained:?}"
        );

        let generation_before = current_generation(&ann, &key).await;
        let query = fnv_to_vec("broken delta chain note inside the published delta", DIMS);
        let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
        match outcome {
            FreshTailOutcome::Replace(candidates, reason) => {
                assert!(
                    candidates.is_empty(),
                    "the stale candidates must be dropped, got: {candidates:?}"
                );
                assert_eq!(
                    reason,
                    Some("fresh-tail: re-resolved segment load failed; dropped stale candidates"),
                    "the drop must disclose its failure site"
                );
            }
            FreshTailOutcome::Ops(ops) => panic!(
                "a floored tail cannot restore a write compacted into the broken \
                 chain; merging it would serve the stale candidates: {ops:?}"
            ),
            FreshTailOutcome::Skipped(reason) => {
                panic!("a skip serves the stale candidates unmerged: {reason}")
            }
        }
        assert!(
            current_generation(&ann, &key).await > generation_before,
            "the drop must force re-adoption so a future query gets a fresh bridge"
        );
    }

    /// A persisted base at `s1` plus a peer's delta checkpoint over it, with
    /// the registry raised to the delta watermark and the log compacted
    /// through it: this process's bridge stays pinned at `s1`, and the write
    /// inside the delta is gone from the retained log.
    struct CompactedPeerDelta {
        rt: TestRuntime,
        ann: SharedAnn,
        key: AnnKey,
        s1: u64,
        delta_s: u64,
        inside: Uuid,
        dir: std::path::PathBuf,
    }

    async fn compacted_peer_delta(model: &str, dims: usize) -> CompactedPeerDelta {
        let rt = test_runtime_with_hash_embedder(model, dims);
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("compacted peer delta seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }
        let ann = new_shared();
        let key = AnnKey::from_token(model);
        ensure_ann_for_model(&rt, &token, &ann, model)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");
        let inside = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "compacted peer delta note inside the published delta",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create delta note")
            .id;

        let dir = ann_segment_dir(&rt, model).expect("segment dir (file-backed test runtime)");
        let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
        let (ops, delta_s) = fetch_final_tail(&rt, model, s1, None)
            .await
            .expect("fetch tail for peer replay");
        assert!(
            delta_s > s1,
            "sanity: the delta must cover a write above s1"
        );
        let raw_count = ops.len() as u64;
        peer_bridge
            .apply_final_ops(ops.clone(), delta_s)
            .expect("apply peer replay");
        peer_bridge.record_delta_batch(ops, delta_s, raw_count);
        assert!(
            !peer_bridge.needs_full_compaction(),
            "sanity: the peer checkpoint must publish a delta, not a full segment"
        );
        delta::write(&dir, &peer_bridge).expect("publish peer delta");
        raise_watermark(&rt, model, delta_s)
            .await
            .expect("raise registry watermark to the delta watermark");
        compact_log(&rt, model).await.expect("compact log");
        let (retained, _) = fetch_final_tail(&rt, model, s1, None)
            .await
            .expect("fetch the retained log above the bridge watermark");
        assert!(
            !retained.iter().any(|(id, _)| *id == inside),
            "precondition: the write inside the delta is gone from the retained \
             log, so no tail can restore it, got: {retained:?}"
        );
        CompactedPeerDelta {
            rt,
            ann,
            key,
            s1,
            delta_s,
            inside,
            dir,
        }
    }

    /// Assert the leg dropped the stale candidates with `expected_reason` and
    /// forced re-adoption.
    async fn assert_dropped_stale_candidates(
        outcome: FreshTailOutcome,
        expected_reason: &'static str,
        fixture: &CompactedPeerDelta,
        generation_before: u64,
    ) {
        match outcome {
            FreshTailOutcome::Replace(candidates, reason) => {
                assert!(
                    candidates.is_empty(),
                    "the stale candidates must be dropped, got: {candidates:?}"
                );
                assert_eq!(
                    reason,
                    Some(expected_reason),
                    "the drop must disclose its failure site"
                );
            }
            FreshTailOutcome::Ops(ops) => panic!(
                "a floored tail cannot restore write {} compacted into the delta; \
                 merging it would serve the stale candidates: {ops:?}",
                fixture.inside
            ),
            FreshTailOutcome::Skipped(reason) => {
                panic!("a skip serves the stale candidates unmerged: {reason}")
            }
        }
        assert!(
            current_generation(&fixture.ann, &fixture.key).await > generation_before,
            "the drop must force re-adoption so a future query gets a fresh bridge"
        );
    }

    /// The mismatch preflight reads the base commit record and the delta HEAD
    /// to decide whether a newer segment exists. A HEAD it cannot read is not
    /// evidence that none does: the leg must drop the stale candidates, never
    /// floor them at the registry minimum and serve them as healthy.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_the_delta_head_cannot_be_read() {
        const MODEL: &str = "adr118-reresolve-unreadable-delta-head-test-model";
        const DIMS: usize = 8;
        let fixture = compacted_peer_delta(MODEL, DIMS).await;
        let base_seq = read_commit_info(&fixture.dir)
            .expect("read base commit")
            .and_then(|info| info.last_applied_seq)
            .expect("base watermark");
        assert_eq!(
            effective_persisted_state(&fixture.dir, base_seq)
                .expect("control: the intact HEAD reads")
                .0,
            fixture.delta_s,
            "control: the intact HEAD promises the delta watermark"
        );

        std::fs::write(fixture.dir.join(delta::HEAD_FILE), b"not a delta head")
            .expect("overwrite the delta HEAD");
        assert!(
            effective_persisted_state(&fixture.dir, base_seq).is_err(),
            "precondition: the preflight's delta HEAD read must fail"
        );

        let generation_before = current_generation(&fixture.ann, &fixture.key).await;
        let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
        let outcome = fresh_tail_leg(
            &fixture.rt,
            &fixture.ann,
            &fixture.key,
            MODEL,
            &query,
            10,
            Some(fixture.s1),
        )
        .await;
        assert_dropped_stale_candidates(
            outcome,
            "fresh-tail: persisted segment state read failed; dropped stale candidates",
            &fixture,
            generation_before,
        )
        .await;
    }

    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_publication_metadata_is_missing() {
        const MODEL: &str = "ann-missing-publication-coverage-test-model";
        const DIMS: usize = 8;
        let fixture = compacted_peer_delta(MODEL, DIMS).await;
        std::fs::remove_file(fixture.dir.join("metadata.bin")).expect("remove fixture metadata");
        assert!(read_commit_info(&fixture.dir).unwrap().is_none());

        let generation_before = current_generation(&fixture.ann, &fixture.key).await;
        let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
        let outcome = fresh_tail_leg(
            &fixture.rt,
            &fixture.ann,
            &fixture.key,
            MODEL,
            &query,
            10,
            Some(fixture.s1),
        )
        .await;
        assert_dropped_stale_candidates(
            outcome,
            "fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates",
            &fixture,
            generation_before,
        )
        .await;
    }

    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_publication_metadata_is_malformed() {
        const MODEL: &str = "ann-malformed-publication-coverage-test-model";
        const DIMS: usize = 8;
        let fixture = compacted_peer_delta(MODEL, DIMS).await;
        std::fs::write(
            fixture.dir.join("metadata.bin"),
            b"invalid fixture commit record",
        )
        .expect("replace fixture metadata");
        assert!(read_commit_info(&fixture.dir).unwrap().is_none());

        let generation_before = current_generation(&fixture.ann, &fixture.key).await;
        let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
        let outcome = fresh_tail_leg(
            &fixture.rt,
            &fixture.ann,
            &fixture.key,
            MODEL,
            &query,
            10,
            Some(fixture.s1),
        )
        .await;
        assert_dropped_stale_candidates(
            outcome,
            "fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates",
            &fixture,
            generation_before,
        )
        .await;
    }

    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_delta_head_is_missing_below_minimum() {
        const MODEL: &str = "ann-missing-head-coverage-test-model";
        const DIMS: usize = 8;
        let fixture = compacted_peer_delta(MODEL, DIMS).await;
        let base_seq = read_commit_info(&fixture.dir)
            .unwrap()
            .and_then(|info| info.last_applied_seq)
            .expect("fixture base watermark");
        assert_eq!(base_seq, fixture.s1);
        assert_eq!(
            effective_persisted_state(&fixture.dir, base_seq).unwrap().0,
            fixture.delta_s
        );
        std::fs::remove_file(fixture.dir.join(delta::HEAD_FILE)).expect("remove fixture HEAD");
        assert_eq!(
            effective_persisted_state(&fixture.dir, base_seq).unwrap().0,
            base_seq
        );
        assert!(
            base_seq < fixture.delta_s,
            "base cannot cover the compacted prefix"
        );

        let generation_before = current_generation(&fixture.ann, &fixture.key).await;
        let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
        let outcome = fresh_tail_leg(
            &fixture.rt,
            &fixture.ann,
            &fixture.key,
            MODEL,
            &query,
            10,
            Some(fixture.s1),
        )
        .await;
        assert_dropped_stale_candidates(
            outcome,
            "fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates",
            &fixture,
            generation_before,
        )
        .await;
    }

    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn segment_classifier_rebuilds_when_delta_head_is_missing_below_active_watermark() {
        const MODEL: &str = "ann-missing-head-rebuild-test-model";
        const DIMS: usize = 8;
        let fixture = compacted_peer_delta(MODEL, DIMS).await;
        std::fs::remove_file(fixture.dir.join(delta::HEAD_FILE)).expect("remove fixture HEAD");
        assert_eq!(
            read_own_watermark(&fixture.rt, MODEL).await.unwrap(),
            Some(fixture.delta_s as i64)
        );
        let restarted = new_shared();
        let mut details = AnnWarmDetails::default();
        let outcome = classify_and_adopt_segment(
            &fixture.rt,
            &restarted,
            &fixture.key,
            MODEL,
            &fixture.dir,
            0,
            0,
            &mut details,
        )
        .await;
        assert!(
            matches!(outcome, SegmentOutcome::Cold),
            "an empty compacted tail cannot make an old base Hot"
        );
        assert!(restarted.indexes.read().await.get(&fixture.key).is_none());

        let token = fixture.rt.authorize(Namespace::local()).unwrap();
        let status = ensure_ann_for_model(&fixture.rt, &token, &restarted, MODEL)
            .await
            .unwrap();
        assert!(matches!(status, AnnEnsureStatus::Built { .. }));
        assert!(bridge_applied_seq(&restarted, &fixture.key).await.unwrap() >= fixture.delta_s);
        let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
        let candidates = search_loaded(&restarted, &fixture.key, &query, 10)
            .await
            .unwrap()
            .unwrap();
        assert!(
            candidates.iter().any(|(id, _)| *id == fixture.inside),
            "full rebuild must recover the fixture write already compacted out of the log"
        );
    }

    /// Re-resolution loads an intact newer segment but its search fails. The
    /// stale candidates still miss the write compacted into the delta, so the
    /// leg must drop them rather than skip and serve them.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_stale_candidates_when_the_re_resolved_search_fails() {
        const MODEL: &str = "adr118-reresolve-search-failure-test-model";
        const DIMS: usize = 8;
        let fixture = compacted_peer_delta(MODEL, DIMS).await;
        let reloaded = AnnBridge::load(&fixture.dir).expect("control: the intact chain loads");
        assert_eq!(
            reloaded.index.last_applied_seq(),
            Some(fixture.delta_s),
            "control: re-resolution would load the delta watermark"
        );
        // A query whose width differs from the index makes the re-resolved
        // search fail after the load succeeds.
        let query = vec![0.5_f32; DIMS + 1];
        assert!(
            reloaded
                .search_with_route(&query, 10, AnnScoreRoute::Memory)
                .is_err(),
            "precondition: the re-resolved search must fail for this query"
        );

        let generation_before = current_generation(&fixture.ann, &fixture.key).await;
        let outcome = fresh_tail_leg(
            &fixture.rt,
            &fixture.ann,
            &fixture.key,
            MODEL,
            &query,
            10,
            Some(fixture.s1),
        )
        .await;
        assert_dropped_stale_candidates(
            outcome,
            "fresh-tail: re-resolved segment search failed; dropped stale candidates",
            &fixture,
            generation_before,
        )
        .await;
    }

    /// A re-resolution that loses its post-search SQL leg must return a reasoned `Replace`, not `None` or a stale-resurrecting `Skipped`.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_post_reresolution_sql_failure_is_a_reasoned_replace() {
        const MODEL: &str = "adr118-reresolve-reasoned-replace-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("reasoned replace seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");

        // A new note lands after the checkpoint; the peer checkpoint below
        // persists it, and its presence in the outcome's candidates is the
        // proof that the REASONED Replace still serves the re-resolved
        // segment rather than falling back to the stale one.
        let fresh = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "reasoned replace distinctive fresh note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create fresh note");

        let (_live, tail_before) = scope_counts(&rt, MODEL, s1)
            .await
            .expect("scope counts before peer checkpoint");
        assert!(tail_before > 0, "sanity: a tail must exist above s1");
        let s2 = s1 + tail_before;

        // Peer checkpoint: persist a segment at s2, raise+compact through it
        // — the mismatch the leg re-resolves from, exactly as in
        // fresh_tail_leg_reresolves_to_a_newer_persisted_segment_on_mismatch.
        let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
        let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
        let (ops, new_s) = fetch_final_tail(&rt, MODEL, s1, None)
            .await
            .expect("fetch tail for peer replay");
        peer_bridge
            .apply_final_ops(ops, new_s)
            .expect("apply peer replay");
        peer_bridge
            .save_atomic(&dir)
            .expect("persist peer checkpoint");
        raise_watermark(&rt, MODEL, s2)
            .await
            .expect("raise registry watermark");
        compact_log(&rt, MODEL).await.expect("compact log");

        // Pause the leg after its segment load+search, before the SQL phase
        // whose failure this test injects.
        ann.reresolve_race_barrier
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let paused = ann.reresolve_race_notify.notified();

        let query = fnv_to_vec("reasoned replace distinctive fresh note", DIMS);
        let handle = tokio::spawn({
            let rt = rt.clone();
            let ann = ann.clone();
            let key = key.clone();
            async move { fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await }
        });
        paused.await;

        // Fault injection: remove the registry table so the leg's re-read —
        // its first post-re-resolution SQL statement — fails. (Test-fixture
        // database, mirroring the DROP TABLE fault pattern in
        // handlers/recall.rs's event-store acquisition test.)
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("fault injection writer");
            w.execute(SqlStatement {
                sql: "DROP TABLE ann_consumer_watermark".into(),
                params: vec![],
                label: Some("test_drop_registry_for_reasoned_replace".into()),
            })
            .await
            .expect("drop registry table");
        }

        ann.reresolve_race_barrier
            .store(false, std::sync::atomic::Ordering::SeqCst);
        ann.reresolve_race_release.notify_one();

        let outcome = handle.await.expect("fresh_tail_leg task");
        match outcome {
            FreshTailOutcome::Replace(candidates, reason) => {
                let reason = reason.expect(
                    "a post-re-resolution SQL failure must be DISCLOSED: \
                     Replace(_, None) reports read-your-writes visibility \
                     the leg no longer proved",
                );
                assert!(
                    reason.starts_with("fresh-tail:"),
                    "the disclosure must name its failure site, got: {reason:?}"
                );
                assert!(
                    candidates.iter().any(|(id, _)| *id == fresh.id),
                    "a reasoned Replace must still serve the re-resolved \
                     segment's candidates (which alone reflect the peer's \
                     checkpoint), got: {candidates:?}"
                );
            }
            FreshTailOutcome::Ops(_) => panic!(
                "expected re-resolution to replace candidates outright, not \
                 return ops against the stale bridge"
            ),
            FreshTailOutcome::Skipped(_) => panic!(
                "a failure AFTER successful re-resolution must not discard \
                 the coherent re-resolved candidates by skipping"
            ),
        }
    }

    /// A peer checkpoint that advances the registry minimum past the just-loaded
    /// segment must make `fresh_tail_reresolve` reload rather than floor the
    /// interleaved window (see `docs/ann.md` for the convergence argument).
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_reresolve_revalidates_registry_minimum_against_interleaved_compaction() {
        const MODEL: &str = "adr118-reresolve-interleave-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("interleave seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");

        // A write lands after the checkpoint — this is what the first peer
        // checkpoint (below) will persist into its segment.
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "interleave first-checkpoint write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create first-checkpoint note");

        let (_live, tail1) = scope_counts(&rt, MODEL, s1)
            .await
            .expect("scope counts before first peer checkpoint");
        assert!(tail1 > 0, "sanity: a tail must exist above s1");
        let s2 = s1 + tail1;

        // Peer checkpoint #1: persist a segment at s2, raise+compact through
        // it. This is the mismatch `fresh_tail_leg` will detect against the
        // stale in-memory bridge still pinned at s1, and `new_s` it will
        // re-resolve to.
        let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
        let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
        let (ops1, applied_s2) = fetch_final_tail(&rt, MODEL, s1, None)
            .await
            .expect("fetch tail for first peer checkpoint");
        peer_bridge
            .apply_final_ops(ops1, applied_s2)
            .expect("apply first peer checkpoint");
        peer_bridge
            .save_atomic(&dir)
            .expect("persist first peer checkpoint");
        raise_watermark(&rt, MODEL, s2)
            .await
            .expect("raise registry watermark to s2");
        compact_log(&rt, MODEL)
            .await
            .expect("compact log through s2");

        // Arm the race: `fresh_tail_reresolve` will pause right after it
        // loads and searches the s2 segment, before it re-validates the
        // registry minimum.
        ann.reresolve_race_barrier
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let paused = ann.reresolve_race_notify.notified();

        let generation_before = current_generation(&ann, &key).await;
        let query = fnv_to_vec("interleave post-compaction write", DIMS);
        let handle = tokio::spawn({
            let rt = rt.clone();
            let ann = ann.clone();
            let key = key.clone();
            async move { fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await }
        });

        // Wait for the leg to reach the armed pause (after its segment load,
        // before its registry re-check) before racing a second checkpoint in.
        paused.await;

        // A further write lands, THEN a peer's second checkpoint covers it:
        // persist a segment at s3, raise+compact through it. This physically
        // removes the (s2, s3] log window — including the write below — from
        // `ann_write_log`, and is exactly the interleaving the fix must
        // detect via its re-validated registry-minimum read.
        let second_checkpoint_write = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "interleave second-checkpoint write",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create second-checkpoint note");
        let (_live, tail2) = scope_counts(&rt, MODEL, s2)
            .await
            .expect("scope counts before second peer checkpoint");
        assert!(tail2 > 0, "sanity: a tail must exist above s2");
        let s3 = s2 + tail2;
        let mut peer_bridge2 =
            AnnBridge::load(&dir).expect("load s2 segment for second checkpoint");
        let (ops2, applied_s3) = fetch_final_tail(&rt, MODEL, s2, None)
            .await
            .expect("fetch tail for second peer checkpoint");
        peer_bridge2
            .apply_final_ops(ops2, applied_s3)
            .expect("apply second peer checkpoint");
        peer_bridge2
            .save_atomic(&dir)
            .expect("persist second peer checkpoint");
        raise_watermark(&rt, MODEL, s3)
            .await
            .expect("raise registry watermark to s3");
        compact_log(&rt, MODEL)
            .await
            .expect("compact log through s3");

        // A write above s3 — still present in the log, not compacted away —
        // must survive the coherent tail scan once the loop converges on
        // the s3 segment.
        let post = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "interleave post-compaction write",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create post-compaction note");

        // Release the paused leg into its (now re-validated) registry-minimum
        // check and tail scan.
        ann.reresolve_race_barrier
            .store(false, std::sync::atomic::Ordering::SeqCst);
        ann.reresolve_race_release.notify_one();

        let outcome = handle.await.expect("fresh_tail_leg task");
        let candidates = match outcome {
            FreshTailOutcome::Replace(candidates, _) => candidates,
            FreshTailOutcome::Ops(_) => panic!(
                "expected re-resolution to replace candidates outright, not \
                 just return ops to merge into the stale bridge's candidates"
            ),
            FreshTailOutcome::Skipped(_) => {
                panic!("expected the interleaved-compaction re-resolution to converge, not a skip")
            }
        };
        assert!(
            candidates
                .iter()
                .any(|(id, _)| *id == second_checkpoint_write.id),
            "a write compacted into the interleaved (s2, s3] window must be \
             recovered by reloading the >= s3 segment on the second round, \
             not silently dropped by a scan floored at s3 with candidates \
             still pinned to the stale s2 segment: {candidates:?}"
        );
        assert!(
            candidates.iter().any(|(id, _)| *id == post.id),
            "a write committed above the converged s3 watermark must \
             survive the coherent tail scan: {candidates:?}"
        );
        assert!(
            current_generation(&ann, &key).await > generation_before,
            "re-resolution must still force re-adoption so a future query \
             gets a fresh bridge"
        );
    }

    /// Exhausting [`FRESH_TAIL_RERESOLVE_MAX_ROUNDS`] under back-to-back peer checkpoints must fall back to the floored scan.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_reresolve_falls_back_to_floor_after_max_rounds() {
        const MODEL: &str = "adr118-reresolve-exhaustion-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..3u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("exhaustion seed note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create seed note");
        }

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s1 = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark after initial warm");

        // First checkpoint (pre-spawn, mirrors the two-round interleave
        // test): this is what `fresh_tail_serving` resolves `new_s` to
        // before ever calling `fresh_tail_reresolve`.
        let write_a = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "exhaustion round-1 write",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create round-1 write");
        let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
        let (_live, tail1) = scope_counts(&rt, MODEL, s1)
            .await
            .expect("scope counts before first checkpoint");
        assert!(tail1 > 0, "sanity: a tail must exist above s1");
        let s2 = s1 + tail1;
        {
            let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
            let (ops, applied) = fetch_final_tail(&rt, MODEL, s1, None)
                .await
                .expect("fetch tail for first checkpoint");
            peer_bridge
                .apply_final_ops(ops, applied)
                .expect("apply first checkpoint");
            peer_bridge
                .save_atomic(&dir)
                .expect("persist first checkpoint");
        }
        raise_watermark(&rt, MODEL, s2)
            .await
            .expect("raise registry watermark to s2");
        compact_log(&rt, MODEL)
            .await
            .expect("compact log through s2");

        ann.reresolve_race_barrier
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut paused = ann.reresolve_race_notify.notified();

        let generation_before = current_generation(&ann, &key).await;
        let query = fnv_to_vec("exhaustion beyond-floor write", DIMS);
        let handle = tokio::spawn({
            let rt = rt.clone();
            let ann = ann.clone();
            let key = key.clone();
            async move { fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await }
        });

        // Two more checkpoints, one per round's pause, each removing that
        // round's gap write from `ann_write_log` — but each gap write is
        // carried forward because the peer checkpoint that compacts it away
        // also folds it into the segment this loop reloads next round.
        let mut prev_s = s2;
        let mut gap_writes = Vec::new();
        for round_idx in 1..=2u32 {
            paused.await;
            let gap_write = rt
                .create_note_with_decay_for_embedding_model(
                    &token,
                    "memory",
                    None,
                    &format!("exhaustion round-{} gap write", round_idx + 1),
                    Some(0.7),
                    0.01,
                    None,
                    vec![],
                    None,
                )
                .await
                .expect("create gap write");
            let (_live, tail) = scope_counts(&rt, MODEL, prev_s)
                .await
                .expect("scope counts before interleaved checkpoint");
            assert!(
                tail > 0,
                "sanity: a tail must exist above the prior watermark"
            );
            let next_s = prev_s + tail;
            {
                let mut peer_bridge = AnnBridge::load(&dir).expect("load segment for checkpoint");
                let (ops, applied) = fetch_final_tail(&rt, MODEL, prev_s, None)
                    .await
                    .expect("fetch tail for interleaved checkpoint");
                peer_bridge
                    .apply_final_ops(ops, applied)
                    .expect("apply interleaved checkpoint");
                peer_bridge
                    .save_atomic(&dir)
                    .expect("persist interleaved checkpoint");
            }
            raise_watermark(&rt, MODEL, next_s)
                .await
                .expect("raise registry watermark");
            compact_log(&rt, MODEL).await.expect("compact log");

            let next_paused = ann.reresolve_race_notify.notified();
            ann.reresolve_race_release.notify_one();
            paused = next_paused;
            gap_writes.push(gap_write);
            prev_s = next_s;
        }

        // Third (terminal-round) checkpoint: its gap write lands in the
        // window the floored fallback cannot see (round == MAX exhausts the
        // loop before it can reload past this checkpoint).
        paused.await;
        let lost_write = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "exhaustion round-4 lost write",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create terminal-round gap write");
        let (_live, tail) = scope_counts(&rt, MODEL, prev_s)
            .await
            .expect("scope counts before terminal checkpoint");
        assert!(
            tail > 0,
            "sanity: a tail must exist above the prior watermark"
        );
        let s_final = prev_s + tail;
        {
            let mut peer_bridge = AnnBridge::load(&dir).expect("load segment for checkpoint");
            let (ops, applied) = fetch_final_tail(&rt, MODEL, prev_s, None)
                .await
                .expect("fetch tail for terminal checkpoint");
            peer_bridge
                .apply_final_ops(ops, applied)
                .expect("apply terminal checkpoint");
            peer_bridge
                .save_atomic(&dir)
                .expect("persist terminal checkpoint");
        }
        raise_watermark(&rt, MODEL, s_final)
            .await
            .expect("raise registry watermark to s_final");
        compact_log(&rt, MODEL)
            .await
            .expect("compact log through s_final");

        // A write above the terminal floor must still surface through the
        // floored fallback's own (still-real) tail scan.
        let beyond_floor = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "exhaustion beyond-floor write",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create beyond-floor write");

        ann.reresolve_race_barrier
            .store(false, std::sync::atomic::Ordering::SeqCst);
        ann.reresolve_race_release.notify_one();

        let outcome = handle.await.expect("fresh_tail_leg task");
        let candidates = match outcome {
            FreshTailOutcome::Replace(candidates, _) => candidates,
            FreshTailOutcome::Ops(_) => panic!(
                "expected the terminal round to replace candidates outright, \
                 not just return ops to merge into the stale bridge's candidates"
            ),
            FreshTailOutcome::Skipped(_) => {
                panic!("expected the bound-exhaustion floored fallback, not a skip")
            }
        };
        assert!(
            candidates.iter().any(|(id, _)| *id == write_a.id),
            "the pre-spawn checkpoint's write must survive every reload: {candidates:?}"
        );
        for gap_write in &gap_writes {
            assert!(
                candidates.iter().any(|(id, _)| *id == gap_write.id),
                "a gap write compacted into a NON-terminal round's reloaded \
                 segment must be recovered, not dropped: {candidates:?}"
            );
        }
        assert!(
            !candidates.iter().any(|(id, _)| *id == lost_write.id),
            "the terminal round's own gap write is the documented \
             ADR-118 mismatch-window loss (its segment is never reloaded \
             once the bound is exhausted) — it must NOT silently reappear \
             here, or this assertion is guarding a fix that changed \
             behavior without updating this test: {candidates:?}"
        );
        assert!(
            candidates.iter().any(|(id, _)| *id == beyond_floor.id),
            "a write committed above the terminal floor must survive the \
             floored fallback's own tail scan: {candidates:?}"
        );
        assert!(
            current_generation(&ann, &key).await > generation_before,
            "the bound-exhaustion floored fallback must still force \
             re-adoption so a future query gets a fresh bridge"
        );
    }

    /// Absent a durable registry row, the leg must not trust `S = 0` as complete — it registers pending and drops stale candidates.
    #[tokio::test]
    #[serial(adr118_fresh_tail)]
    async fn fresh_tail_leg_drops_candidates_and_reregisters_when_consumer_row_absent() {
        const MODEL: &str = "adr118-registration-precondition-test-model";
        const DIMS: usize = 8;
        let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "registration precondition seed note",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");

        let ann = new_shared();
        let key = AnnKey::from_token(MODEL);
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
            .await
            .expect("warm");
        let s = bridge_applied_seq(&ann, &key)
            .await
            .expect("bridge watermark");

        // Delete this consumer's durable registry row directly, simulating a
        // process that has never registered (or whose row was reset).
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: "DELETE FROM ann_consumer_watermark \
                      WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                    .into(),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(ANN_WILDCARD_NS.into()),
                    SqlValue::Text(MODEL.into()),
                ],
                label: Some("test_delete_consumer_watermark_row".into()),
            })
            .await
            .expect("delete registry row");
        }
        assert!(
            read_own_watermark(&rt, MODEL)
                .await
                .expect("read watermark")
                .is_none(),
            "sanity: the registry row must be gone before the leg runs"
        );

        let query = fnv_to_vec("registration precondition seed note", DIMS);
        let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await;
        match outcome {
            FreshTailOutcome::Replace(hits, reason) => {
                assert!(
                    hits.is_empty(),
                    "the exact leg must discard candidates captured before registry loss"
                );
                assert!(
                    reason.is_some_and(|r| !r.is_empty()),
                    "dropping candidates is a degraded serve and must carry a \
                     failure-site reason for disclosure"
                );
            }
            _ => panic!(
                "the exact leg must drop candidates when this consumer's durable \
                 registration row is absent"
            ),
        }
        assert!(
            search_loaded_with_seq(&ann, &key, &query, 10)
                .await
                .expect("search after registry loss")
                .is_none(),
            "registry loss must evict the unprotected in-process bridge"
        );
        assert_eq!(
            read_own_watermark(&rt, MODEL)
                .await
                .expect("read watermark"),
            Some(PENDING_WATERMARK),
            "the leg must re-register this consumer in the closed pending state"
        );
    }
}

#[cfg(test)]
#[path = "ann/bridge_incremental_tests.rs"]
mod bridge_incremental_tests;
