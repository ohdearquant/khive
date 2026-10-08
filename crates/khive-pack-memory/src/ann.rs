//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.
//! See `crates/khive-pack-memory/docs/api/ann-lifecycle.md` for lifecycle and race handling,
//! and `crates/khive-pack-memory/docs/ann.md` for the restart classifier and ADR-118 design.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_retrieval::ann::corpus::{CorpusScope, LiveRowJoin, WatermarkCapture};
use khive_retrieval::ann::registry::{self as ann_registry, WatermarkAuthority, PENDING_WATERMARK};
use khive_runtime::config::ann_rebuild_threshold_from_env as ann_rebuild_threshold;
use khive_runtime::{
    is_benign_shutdown_cancellation, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::StorageError;
use khive_vamana::bridge::AnnBridgeCore;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{read_commit_info, segment_commit_digest, CorpusFingerprint};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

// Reached by the test modules through `use super::*`.
#[cfg(test)]
use khive_vamana::write_external_ids_sidecar;

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
#[path = "ann/final_tail.rs"]
mod final_tail;
use final_tail::fetch_final_tail_on;
#[cfg(test)]
#[path = "ann/rotation_watch_test_support.rs"]
mod rotation_watch_test_support;
#[cfg(test)]
pub(crate) use rotation_watch_test_support::take_rotation_watch_handle_for_test;

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
    /// The index, its id map and its commit digest; they read as fields of the
    /// bridge through `Deref`.
    core: AnnBridgeCore,
    incarnation: Arc<()>,
    #[cfg(test)]
    reverse_map_scan_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Built on first replay; subsequent batches update only changed subjects.
    reverse_map: Option<HashMap<Uuid, u32>>,
    #[cfg(test)]
    reverse_map_builds: usize,
    dirty_ops: u64,
    published_seq: u64,
    last_checkpoint: std::time::Instant,
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

impl std::ops::Deref for AnnBridge {
    type Target = AnnBridgeCore;

    fn deref(&self) -> &AnnBridgeCore {
        &self.core
    }
}

impl std::ops::DerefMut for AnnBridge {
    fn deref_mut(&mut self) -> &mut AnnBridgeCore {
        &mut self.core
    }
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
    /// The watcher task started for this state, retained so a lifecycle test
    /// can observe this state's own watcher instead of a process-wide counter.
    #[cfg(test)]
    rotation_watch_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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
        rotation_watch_handle: std::sync::Mutex::new(None),
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
    [::khive_runtime::sql!("memory_ann_epoch_create")];

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
            sql: ::khive_runtime::sql!("memory_ann_epoch_select").into(),
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
        sql: ::khive_runtime::sql!("memory_ann_epoch_increment").into(),
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
        vectors: Vec<f32>,
        dim: usize,
        id_map: Vec<Uuid>,
        namespace_set: HashSet<String>,
    ) -> Result<Self, RuntimeError> {
        let core = AnnBridgeCore::build(vectors, dim, id_map).map_err(RuntimeError::Internal)?;
        // The core accepted the build only when the vector count equals the id-map length.
        let n = core.id_map.len();
        Ok(Self {
            core,
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: None,
            reverse_map: None,
            #[cfg(test)]
            reverse_map_builds: 0,
            dirty_ops: 0,
            published_seq: 0,
            last_checkpoint: std::time::Instant::now(),
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
        let raw = self
            .core
            .search_hits(query, k)
            .map_err(|e| RuntimeError::Internal(format!("memory ANN search: {e}")))?;
        let mut hits = Vec::with_capacity(raw.len());
        for (uuid, dist) in raw {
            // The graph emits normalized-vector L2². For note search,
            // L2²/2 is cosine distance and uses the same converter as the
            // exact sqlite-vec route; memory recall retains its prior floor.
            hits.push((uuid, route.graph_score(dist)?));
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
            core: AnnBridgeCore {
                index: self.index.fork_for_maintenance(),
                id_map: self.id_map.clone(),
                commit_digest: self.commit_digest,
            },
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: self.reverse_map_scan_hook.clone(),
            reverse_map: self.reverse_map.clone(),
            #[cfg(test)]
            reverse_map_builds: self.reverse_map_builds,
            dirty_ops: self.dirty_ops,
            published_seq: self.published_seq,
            last_checkpoint: self.last_checkpoint,
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
        // Disjoint field borrows: `Deref` would borrow the whole bridge while `reverse` is live.
        let core = &mut self.core;
        let reverse = self.reverse_map.as_mut().expect("initialized reverse map");

        for (uuid, op) in ops {
            match op {
                None => {
                    if let Some(&ordinal) = reverse.get(&uuid) {
                        // Fail closed: if a same-batch upsert already reused
                        // this slot, skip the tombstone instead of deleting
                        // someone else's live vector.
                        if core.id_map.get(ordinal as usize) != Some(&uuid) {
                            tracing::warn!(
                                subject = %uuid,
                                ordinal,
                                "replay delete: ordinal reassigned within batch, skipping tombstone"
                            );
                            reverse.remove(&uuid);
                            continue;
                        }
                        core.index
                            .tombstone(ordinal)
                            .map_err(|e| format!("replay tombstone({ordinal}): {e}"))?;
                        reverse.remove(&uuid);
                    }
                }
                Some(mut embedding) => {
                    l2_normalize(&mut embedding);
                    if let Some(&old) = reverse.get(&uuid) {
                        if core.id_map.get(old as usize) != Some(&uuid) {
                            return Err(format!(
                                "replay upsert: ordinal {old} is no longer owned by {uuid}"
                            ));
                        }
                        core.index
                            .tombstone(old)
                            .map_err(|e| format!("replay tombstone({old}): {e}"))?;
                        reverse.remove(&uuid);
                    }
                    let ordinal = core
                        .index
                        .insert(&embedding)
                        .map_err(|e| format!("replay insert: {e}"))?;
                    let slot = ordinal as usize;
                    match slot.cmp(&core.id_map.len()) {
                        std::cmp::Ordering::Less => {
                            let previous_owner = core.id_map[slot];
                            if reverse.get(&previous_owner) == Some(&ordinal) {
                                reverse.remove(&previous_owner);
                            }
                            core.id_map[slot] = uuid;
                        }
                        std::cmp::Ordering::Equal => core.id_map.push(uuid),
                        std::cmp::Ordering::Greater => {
                            return Err(format!(
                                "replay insert returned ordinal {ordinal} beyond id_map len {}",
                                core.id_map.len()
                            ));
                        }
                    }
                    reverse.insert(uuid, ordinal);
                }
            }
        }
        core.index.set_last_applied_seq(Some(new_s));
        Ok(())
    }

    pub(crate) fn consolidate_if_needed(&mut self, tau: usize) -> Result<bool, String> {
        let core = &mut self.core;
        if !core.index.needs_consolidation() && core.index.ops_since_consolidation() < tau {
            return Ok(false);
        }
        if core.id_map.len() != core.index.num_vectors() {
            return Err("consolidation: id_map length differs from vector count".to_string());
        }
        let new_to_old = core
            .index
            .consolidate()
            .map_err(|e| format!("memory ANN consolidation: {e}"))?;
        if !new_to_old.is_empty() {
            core.id_map = new_to_old
                .into_iter()
                .map(|old| {
                    core.id_map.get(old as usize).copied().ok_or_else(|| {
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
        let digest = self.core.save_atomic(dir)?;
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
        let (core, commit_digest) = AnnBridgeCore::load(dir)?;
        let base_applied_seq = core.index.last_applied_seq().unwrap_or(0);
        let base_ops = core.index.num_vectors();
        let mut bridge = Self {
            core,
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: None,
            reverse_map: None,
            #[cfg(test)]
            reverse_map_builds: 0,
            dirty_ops: 0,
            published_seq: base_applied_seq,
            last_checkpoint: std::time::Instant::now(),
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
    let shutdown = khive_runtime::daemon_shutdown_token();
    let watcher = start_rotation_watcher_with_shutdown(rt, ann, shutdown);
    #[cfg(test)]
    rotation_watch_test_support::retain_rotation_watch_handle(ann, watcher);
    #[cfg(not(test))]
    drop(watcher);
}

fn start_rotation_watcher_with_shutdown(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    shutdown: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let ann_root = rt.backend_ann_root()?;
    let watch = khive_retrieval::ann::rotation_watch_future(
        ann,
        &ann.rotation_watch_started,
        ann_root,
        ROTATION_WATCH_INTERVAL,
        shutdown,
        |ann, ann_root| async move {
            refresh_rotated_segments_in_root(&ann_root, &ann).await;
        },
    )?;
    Some(khive_runtime::spawn_named_tracked_task(
        "memory_ann_rotation_watch",
        watch,
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
        .query_all(memory_corpus().fingerprint(&table_name, model, "memory_ann_fingerprint"))
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
        .query_all(memory_corpus().corpus_scan(&table_name, model, "memory_ann_corpus_scan"))
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
    ann_registry::register_pending_dispatch(
        sql.as_ref(),
        "memory_",
        consumer,
        ANN_WILDCARD_NS,
        model,
        ann_segment_dir(rt, model).is_none(),
    )
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
    ann_registry::read_watermark(sql.as_ref(), "memory_", consumer, ANN_WILDCARD_NS, model)
        .await
        .map_err(|e| e.to_string())
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
    let pathless = ann_segment_dir(rt, model).is_none();
    if pathless {
        i64::try_from(s)
            .map_err(|_| format!("memory ANN watermark {s} exceeds SQLite INTEGER range"))?;
    }
    let raised = ann_registry::raise_watermark_dispatch(
        sql.as_ref(),
        "memory_",
        consumer,
        ANN_WILDCARD_NS,
        model,
        s,
        authority,
        pathless,
    )
    .await
    .map_err(|e| e.to_string())?;
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

fn memory_corpus() -> CorpusScope<'static> {
    CorpusScope {
        namespace: None,
        record_kind: Some("note"),
        field: "note.content",
        live_join: Some(LiveRowJoin::Notes),
        watermark_capture: WatermarkCapture::ScopedMaximumWithOwnFloor {
            consumer: ANN_CONSUMER,
            registry_namespace: ANN_WILDCARD_NS,
        },
    }
}

/// Compact the write log for `model` across every namespace, each bounded by
/// its own wildcard-inclusive registry minimum (ADR-079 Amendment 1 §A step
/// 3). A namespace with no registered rows yields `seq <= NULL`, which
/// matches nothing.
async fn compact_log(rt: &KhiveRuntime, model: &str) -> Result<(), String> {
    let sql = rt.sql();
    ann_registry::compact_dispatch(
        sql.as_ref(),
        "memory_",
        memory_corpus().compaction_scope(),
        model,
        ann_segment_dir(rt, model).is_none(),
    )
    .await
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
        .query_all(memory_corpus().scope_counts(&table_name, model, s, "memory_ann_scope_counts"))
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
        .query_all(memory_corpus().tail_exists(model, s, "memory_ann_tail_exists"))
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
            sql: ::khive_runtime::sql!("memory_ann_registry_min_select").into(),
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
            sql: ::khive_runtime::sql!("memory_ann_consumer_watermark_select").into(),
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
/// non-negative floor. An empty query or a length mismatch scores 0.0.
pub(crate) fn exact_cosine(query: &[f32], embedding: &[f32]) -> f32 {
    if query.len() != embedding.len() || query.is_empty() {
        return 0.0;
    }
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
        let score = khive_score::try_cosine_score_with_f32_tolerance(distance)
            .map_err(|_| format!("session exact cosine distance out of range: {distance}"))?
            .to_f64() as f32;
        candidates.push((id, score));
    }
    Ok(Some(candidates))
}

/// Merge a fresh-tail's coalesced final ops into an ANN candidate list
/// (ADR-118 §2): deduplicated by `subject_id` with the tail winning, then
/// re-sorted by score, with equal scores in ascending id order (ADR-079
/// Amendment 4, item 6). A `None` op (delete) drops the subject even if it was
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
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
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
#[path = "ann_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ann/bridge_incremental_tests.rs"]
mod bridge_incremental_tests;

#[cfg(test)]
#[path = "ann/corpus_statement_tests.rs"]
mod corpus_statement_tests;

#[cfg(test)]
#[path = "ann/corpus_capture_tests.rs"]
mod corpus_capture_tests;
