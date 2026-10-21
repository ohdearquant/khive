use super::*;

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
    pub(super) core: AnnBridgeCore,
    pub(super) incarnation: Arc<()>,
    #[cfg(test)]
    pub(super) reverse_map_scan_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Built on first replay; subsequent batches update only changed subjects.
    pub(super) reverse_map: Option<HashMap<Uuid, u32>>,
    #[cfg(test)]
    pub(super) reverse_map_builds: usize,
    pub(super) dirty_ops: u64,
    pub(super) published_seq: u64,
    pub(super) last_checkpoint: std::time::Instant,
    /// The stable v2 commit remains unchanged while memory-owned delta
    /// publications advance their own nonce and watermark beside it.
    pub(super) base_commit_digest: Option<[u8; 32]>,
    pub(super) base_applied_seq: u64,
    pub(super) base_ops: usize,
    pub(super) delta_batches: Vec<delta::DeltaBatch>,
    pub(super) delta_raw_ops: u64,
    pub(super) delta_chunks: usize,
    pub(super) last_delta_nonce: Option<Uuid>,
    /// Indexed namespaces, used to skip unnecessary recall over-fetch retries.
    pub(crate) namespace_set: HashSet<String>,
    /// In-process write generation captured before this build's corpus scan.
    pub(crate) generation: u64,
    /// Durable corpus epoch observed at build or snapshot load time.
    pub(crate) epoch_baseline: u64,
    /// Lets rotation tests prove replacement drops the bridge that owns the
    /// predecessor mmaps, rather than merely changing the cache metadata.
    #[cfg(test)]
    pub(super) drop_probe: Option<Arc<()>>,
}

khive_vamana::impl_ann_bridge_helpers!(
    AnnBridge,
    /// Stamps the build with the write generation represented by its corpus snapshot.
);

#[cfg(test)]
#[test]
pub(super) fn note_search_score_carrier_preserves_non_cardinal_fixed_point_bits() {
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
    pub(super) note_search_consumer_enabled: AtomicBool,
    pub(super) indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    pub(super) checkpoint_policy: std::sync::RwLock<CheckpointPolicy>,
    pub(super) checkpoint_timers: std::sync::Mutex<HashSet<AnnKey>>,
    pub(super) checkpoint_timers_enabled: bool,
    pub(super) owner_dropped: tokio_util::sync::CancellationToken,
    #[cfg(test)]
    pub(super) segment_load_count: AtomicUsize,
    #[cfg(test)]
    pub(super) fail_next_segment_load: AtomicBool,
    #[cfg(test)]
    pub(super) publication_count: AtomicUsize,
    /// Synchronous so `WarmingGuard::drop` can release it on every exit path.
    pub(super) warming: std::sync::Mutex<HashSet<AnnKey>>,
    /// Per-model warm lock shared by boot, background, and cold-recall paths.
    pub(super) model_locks: Mutex<HashMap<AnnKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Monotonic per-model write generations used to reject stale installs.
    pub(super) generations: Mutex<HashMap<AnnKey, u64>>,
    /// Last durable-epoch query per model, used to debounce warm-hit checks.
    pub(super) last_epoch_check: std::sync::Mutex<HashMap<AnnKey, std::time::Instant>>,
    /// Idempotence guard for the pack-lifetime file-generation watcher.
    pub(super) rotation_watch_started: AtomicBool,
    /// The watcher task started for this state, retained so a lifecycle test
    /// can observe this state's own watcher instead of a process-wide counter.
    #[cfg(test)]
    pub(super) rotation_watch_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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

impl Drop for AnnState {
    fn drop(&mut self) {
        self.owner_dropped.cancel();
    }
}

pub(crate) fn enable_note_search_consumer(ann: &SharedAnn) {
    ann.note_search_consumer_enabled
        .store(true, Ordering::Release);
}

pub(super) fn note_search_consumer_enabled(ann: &SharedAnn) -> bool {
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
        owner_dropped: tokio_util::sync::CancellationToken::new(),
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
pub(super) async fn current_generation(ann: &SharedAnn, key: &AnnKey) -> u64 {
    ann.generations.lock().await.get(key).copied().unwrap_or(0)
}

/// Whether an installed graph satisfies the caller's minimum generation.
pub(super) async fn installed_is_fresh(ann: &SharedAnn, key: &AnnKey, min_generation: u64) -> bool {
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
pub(super) const REBUILD_CHAIN_DEBOUNCE_DEFAULT: std::time::Duration =
    std::time::Duration::from_secs(30);
#[cfg(test)]
pub(super) const REBUILD_CHAIN_DEBOUNCE_DEFAULT: std::time::Duration =
    std::time::Duration::from_millis(5);

pub(super) fn rebuild_chain_debounce() -> std::time::Duration {
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
pub(super) fn resolve_rebuild_chain_debounce(
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
pub(super) const ROTATION_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Return a model's warm lock without holding the lock-map mutex across warming.
pub(super) async fn model_warm_lock(ann: &SharedAnn, key: &AnnKey) -> Arc<tokio::sync::Mutex<()>> {
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
