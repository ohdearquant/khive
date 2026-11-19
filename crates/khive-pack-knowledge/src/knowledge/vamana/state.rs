use super::*;

pub(crate) struct AnnBridge {
    /// The index, its id map and its commit digest; they read as fields of the
    /// bridge through `Deref`.
    pub(super) core: AnnBridgeCore,
    /// Namespace write-generation this build's corpus scan started at or after
    /// (issue #770). Stamped just before install; `install_if_fresher` uses it
    /// to reject a late-arriving build whose scan predates a `clear_namespace`
    /// invalidation that landed while it was still running.
    pub(super) generation: u64,
    /// Test-only proof that replacing this bridge drops its mmap owner.
    #[cfg(test)]
    pub(super) drop_probe: Option<Arc<()>>,
    #[cfg(test)]
    pub(super) search_pause: Option<Arc<TestSearchPause>>,
}

khive_vamana::impl_ann_bridge_helpers!(
    AnnBridge,
    /// Stamp this bridge with the namespace write-generation its corpus scan
    /// started at or after (issue #770). Called just before install; see
    /// `install_if_fresher`.
);

#[cfg(test)]
#[derive(Default)]
pub(super) struct TestSearchPause {
    pub(super) started: AtomicBool,
    pub(super) released: std::sync::Mutex<bool>,
    pub(super) wake: std::sync::Condvar,
}

#[cfg(test)]
impl TestSearchPause {
    pub(super) fn wait(&self) {
        let mut released = self.released.lock().expect("search pause");
        self.started.store(true, Ordering::SeqCst);
        while !*released {
            released = self.wake.wait(released).expect("search pause");
        }
    }

    pub(super) fn release(&self) {
        *self.released.lock().expect("search pause") = true;
        self.wake.notify_all();
    }
}

/// Cache key for a per-{namespace, model} ANN index slot.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct AnnKey {
    pub(super) namespace: String,
    pub(super) model: String,
}

impl AnnKey {
    pub(crate) fn new(namespace: &str, model: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            model: model.to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AnnWarmFailure {
    EmptyCorpus,
    Operational,
    Interrupted,
    /// Not the warm index host; the build belongs to the daemon.
    NotWarmHost,
}

/// Result of one load/rebuild worker. Kept separate from `AnnWarmState` so a
/// failed replacement can remain retryable even when ADR-079 rule 8 left a
/// stale-but-servable bridge installed during the attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnWarmOutcome {
    Ready,
    Empty,
    Failed,
    /// This process is not the warm index host and the only way forward was a
    /// corpus-scale build. Retryable exactly like `Failed`: the caller serves
    /// its degraded path for this request and the daemon builds.
    Declined,
}

/// Lifecycle for one per-{namespace, model} warm slot.
///
/// `Failed` is deliberately retryable: an empty corpus may become populated,
/// and an operational load failure says nothing about the next attempt. A
/// namespace invalidation removes every matching state, returning those keys
/// to the implicit `Absent` state.
#[derive(Debug)]
pub(super) enum AnnWarmState {
    Warming {
        attempt_id: u64,
        generation: u64,
        started_at: std::time::Instant,
    },
    Ready {
        generation: u64,
    },
    Failed {
        generation: u64,
        error: AnnWarmFailure,
    },
}

/// Ownership token for one warm attempt. Only the matching attempt may
/// transition its slot out of `Warming`, so a late completion cannot erase or
/// complete a newer post-invalidation warm.
pub(super) struct AnnWarmPermit {
    pub(super) ann: SharedAnn,
    pub(super) key: AnnKey,
    pub(super) attempt_id: u64,
    pub(super) generation: u64,
    pub(super) finished: bool,
}

/// Shared ANN state: per-{namespace, model} indexes plus a warm lifecycle that
/// permits at most one load/rebuild attempt per key at a time.
pub(crate) struct AnnState {
    pub(super) indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    /// Per-key warm lifecycle. `std::sync::Mutex` keeps `begin_warm` usable by
    /// the fire-and-return query path before it spawns an async task.
    pub(super) warm_states: std::sync::Mutex<HashMap<AnnKey, AnnWarmState>>,
    /// Monotonic ownership token for warm attempts. Generation alone cannot
    /// distinguish a failed retry from its predecessor at the same generation.
    pub(super) next_warm_attempt_id: AtomicU64,
    /// Per-namespace write-generation counter (issue #770), keyed by
    /// namespace (not the full `AnnKey`). Bumped by `clear_namespace`;
    /// `install_if_fresher` uses it to reject stale builds. See
    /// crates/khive-pack-knowledge/docs/api/vamana.md#annstategenerations-per-namespace-write-generation-counter-issue-770.
    pub(super) generations: std::sync::Mutex<HashMap<String, u64>>,
    /// Keys whose most recent corpus scan completed and found nothing
    /// buildable (empty corpus), mapped to the namespace write-generation
    /// captured at scan start (issue #1026). Only `Ok(None)` scans mark:
    /// a rebuild error is operational (store open, SQL reader, corpus
    /// query) and says nothing about the corpus, so error paths keep the
    /// bounded-wait retry behavior instead of a marker. A marker is
    /// terminal — `wait_ready` returns immediately rather than polling out
    /// `ANN_WARM_WAIT_TIMEOUT_MS` — exactly when its stored generation is
    /// still >= the namespace's CURRENT generation: nothing can have changed
    /// the outcome since the scan that produced it. A marker whose stored
    /// generation has fallen behind means the corpus mutated after the scan,
    /// so it no longer predicts anything and is discarded on next check.
    /// `install_if_fresher` clears a key's marker whenever it actually
    /// installs a fresh index for that key.
    pub(super) unavailable: std::sync::Mutex<HashMap<AnnKey, u64>>,
    /// Keys whose durable consumer registration disappeared while an index
    /// was serving.  Such a bridge is untrusted because another consumer may
    /// already have compacted part of its tail (ADR-118 registration
    /// precondition).  The marker is installed before re-registration and is
    /// cleared only after an authoritative full-corpus scan publishes at the
    /// current namespace generation.
    pub(super) force_rebuild: std::sync::Mutex<HashSet<AnnKey>>,
    /// Process-local half of checkpoint publication serialization. File-backed
    /// runtimes additionally take the directory lock for cross-process safety;
    /// pathless runtimes need this lock to linearize raise-then-install.
    pub(super) checkpoint_locks:
        std::sync::Mutex<HashMap<AnnKey, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// Idempotence guard for the pack-lifetime file-generation watcher.
    pub(super) rotation_watch_started: AtomicBool,
    /// Whether this process may build an ANN index from the full corpus and
    /// publish the result. A corpus build is minutes of CPU and a segment
    /// rewrite every other reader on the index root must then absorb, and it
    /// pays for itself only in a process that outlives the request. Serving
    /// processes set this from the daemon role at construction; the admin
    /// reindex path sets it unconditionally, because building is what it was
    /// invoked to do. A process without it serves what is already persisted,
    /// or serves degraded and leaves the build to the daemon.
    pub(super) builds_corpus_indexes: bool,
    /// Test-only rendezvous for `finish_warm`'s Ready publish (issue #2340
    /// regression coverage) — see `run_finish_warm_ready_test_hook`.
    #[cfg(test)]
    pub(super) test_finish_warm_ready_hook: std::sync::Mutex<Option<Arc<FinishWarmReadyTestHook>>>,
}

/// Test-only hook installed on an `AnnState` to drive a concurrent eviction
/// attempt from inside `finish_warm`'s Ready critical section. See
/// `run_finish_warm_ready_test_hook`.
#[cfg(test)]
pub(crate) struct FinishWarmReadyTestHook {
    pub(crate) incumbent_digest: [u8; 32],
    pub(crate) generation: u64,
    pub(crate) handle_tx: tokio::sync::mpsc::UnboundedSender<tokio::task::JoinHandle<()>>,
}

pub(crate) type SharedAnn = Arc<AnnState>;

/// Shared ANN state for a process that builds corpus indexes: the warm daemon
/// and the admin reindex path.
pub(crate) fn new_shared() -> SharedAnn {
    new_shared_for_role(true)
}

/// Shared ANN state whose corpus-build authority is stated explicitly. Serving
/// packs pass the daemon role; see `AnnState::builds_corpus_indexes`.
pub(crate) fn new_shared_for_role(builds_corpus_indexes: bool) -> SharedAnn {
    Arc::new(AnnState {
        builds_corpus_indexes,
        indexes: RwLock::new(HashMap::new()),
        warm_states: std::sync::Mutex::new(HashMap::new()),
        next_warm_attempt_id: AtomicU64::new(1),
        generations: std::sync::Mutex::new(HashMap::new()),
        unavailable: std::sync::Mutex::new(HashMap::new()),
        force_rebuild: std::sync::Mutex::new(HashSet::new()),
        checkpoint_locks: std::sync::Mutex::new(HashMap::new()),
        rotation_watch_started: AtomicBool::new(false),
        #[cfg(test)]
        test_finish_warm_ready_hook: std::sync::Mutex::new(None),
    })
}

pub(super) fn force_rebuild_guard(
    m: &std::sync::Mutex<HashSet<AnnKey>>,
) -> std::sync::MutexGuard<'_, HashSet<AnnKey>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn mark_force_rebuild(ann: &SharedAnn, key: &AnnKey) {
    force_rebuild_guard(&ann.force_rebuild).insert(key.clone());
}

pub(super) fn force_rebuild_required(ann: &SharedAnn, key: &AnnKey) -> bool {
    force_rebuild_guard(&ann.force_rebuild).contains(key)
}

pub(super) fn clear_force_rebuild_if_current(ann: &SharedAnn, key: &AnnKey, generation: u64) {
    if current_generation(ann, &key.namespace) == generation {
        clear_force_rebuild(ann, key);
    }
}

pub(super) fn clear_force_rebuild(ann: &SharedAnn, key: &AnnKey) {
    force_rebuild_guard(&ann.force_rebuild).remove(key);
}

pub(super) fn checkpoint_lock(ann: &SharedAnn, key: &AnnKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = ann
        .checkpoint_locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key.clone(), Arc::downgrade(&lock));
    lock
}

// Recover a poisoned generations Mutex rather than aborting: the guarded
// HashMap<String, u64> stays logically valid through a poison (worst case a
// stale reader misses one bump, which only widens — never narrows — the set
// of builds treated as possibly-stale).
pub(super) fn generations_guard(
    m: &std::sync::Mutex<HashMap<String, u64>>,
) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bump `namespace`'s write-generation counter and return the new value
/// (issue #770). Called from `clear_namespace`, the single chokepoint every
/// corpus-mutating write already routes through.
pub(super) fn bump_generation(ann: &SharedAnn, namespace: &str) -> u64 {
    let mut gens = generations_guard(&ann.generations);
    let slot = gens.entry(namespace.to_owned()).or_insert(0);
    *slot += 1;
    *slot
}

/// Read `namespace`'s current write-generation counter (0 if never bumped).
pub(crate) fn current_generation(ann: &SharedAnn, namespace: &str) -> u64 {
    generations_guard(&ann.generations)
        .get(namespace)
        .copied()
        .unwrap_or(0)
}

/// Install `candidate` into the cache for `key` unless it is stale (PR #815,
/// covering issue #770's empty-slot scenario). Two independent fences, both
/// evaluated while holding the write lock: candidate's generation must be at
/// least the namespace's CURRENT generation (not just any already-installed
/// entry's, since `clear_namespace` may have emptied the slot entirely), AND
/// at least any already-installed entry's generation, so a slower-but-staler
/// build can never clobber a faster build that scanned a newer corpus. See
/// crates/khive-pack-knowledge/docs/api/vamana.md#install_if_fresher-pr-815-covering-issue-770s-empty-slot-scenario.
pub(crate) async fn install_if_fresher(ann: &SharedAnn, key: &AnnKey, candidate: AnnBridge) {
    let mut idxs = ann.indexes.write().await;

    let ns_generation = current_generation(ann, &key.namespace);
    if candidate.generation < ns_generation {
        tracing::debug!(
            key = ?key,
            candidate_generation = candidate.generation,
            namespace_generation = ns_generation,
            "knowledge ANN install skipped: candidate predates namespace's current generation"
        );
        return;
    }

    match idxs.get(key) {
        Some(existing) if existing.generation >= candidate.generation => {
            tracing::debug!(
                key = ?key,
                existing_generation = existing.generation,
                candidate_generation = candidate.generation,
                "knowledge ANN install skipped: cached entry already >= this build's generation"
            );
        }
        _ => {
            idxs.insert(key.clone(), candidate);
            unavailable_guard(&ann.unavailable).remove(key);
        }
    }
}

/// Install `candidate` over whatever the slot holds.
///
/// The only check is the namespace-generation fence `install_if_fresher` also
/// applies: a candidate that predates the namespace's current generation is
/// rejected and `false` is returned. The incumbent's generation is never read,
/// so an accepted candidate replaces an older, an equal and a newer incumbent
/// alike, and clears the key's unavailable marker.
///
/// Callers are ordered by the per-key lock from `checkpoint_lock`, which each
/// of them holds across the call:
///
/// - `checkpoint_raise_compact_readopt`, called from the warm path and from
///   the index verb handler, installs the bridge it was handed or the reopened
///   segment it published;
/// - `adopt_checkpoint_winner`, reached only from that function, installs the
///   segment another checkpoint already published;
/// - `refresh_rotated_segment`, run by the rotation watcher, installs a newly
///   published segment in place of the incumbent it supersedes.
///
/// `install_if_fresher` keeps the incumbent on a tie because its callers can
/// race one another. The callers here are serialized by that lock, so the
/// later install wins.
pub(crate) async fn install_replacing(ann: &SharedAnn, key: &AnnKey, candidate: AnnBridge) -> bool {
    let mut idxs = ann.indexes.write().await;
    let ns_generation = current_generation(ann, &key.namespace);
    if candidate.generation < ns_generation {
        tracing::debug!(
            key = ?key,
            candidate_generation = candidate.generation,
            namespace_generation = ns_generation,
            "knowledge ANN replace skipped: candidate predates namespace's current generation"
        );
        return false;
    }
    idxs.insert(key.clone(), candidate);
    unavailable_guard(&ann.unavailable).remove(key);
    true
}

pub(super) async fn has_current_index(ann: &SharedAnn, key: &AnnKey) -> bool {
    let idxs = ann.indexes.read().await;
    let current = current_generation(ann, &key.namespace);
    idxs.get(key)
        .is_some_and(|bridge| bridge.generation >= current)
}

pub(super) async fn has_current_index_at_watermark(
    ann: &SharedAnn,
    key: &AnnKey,
    watermark: u64,
) -> bool {
    let idxs = ann.indexes.read().await;
    let current = current_generation(ann, &key.namespace);
    idxs.get(key).is_some_and(|bridge| {
        bridge.generation >= current && bridge.index.last_applied_seq().unwrap_or(0) >= watermark
    })
}

// Recover a poisoned warm-state Mutex rather than aborting: each transition is
// one HashMap replacement, so the previous or next complete state remains safe
// to inspect after a poison.
pub(super) fn warm_states_guard(
    m: &std::sync::Mutex<HashMap<AnnKey, AnnWarmState>>,
) -> std::sync::MutexGuard<'_, HashMap<AnnKey, AnnWarmState>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Claim the single warm slot for `key`.
///
/// `Warming` and `Ready` at the current namespace generation suppress a
/// duplicate attempt. `Failed`, `Absent`, and stale-generation states all
/// transition to a newly owned `Warming` attempt so empty/operational failures
/// remain retryable after the current request degrades.
pub(super) fn begin_warm(ann: &SharedAnn, key: AnnKey) -> Option<AnnWarmPermit> {
    let mut states = warm_states_guard(&ann.warm_states);
    let generation = current_generation(ann, &key.namespace);

    match states.get(&key) {
        Some(
            AnnWarmState::Warming {
                generation: state_generation,
                ..
            }
            | AnnWarmState::Ready {
                generation: state_generation,
            },
        ) if *state_generation >= generation => return None,
        Some(AnnWarmState::Failed {
            generation: failed_generation,
            error,
        }) => {
            tracing::debug!(
                key = ?key,
                failed_generation,
                error = ?error,
                "retrying failed knowledge ANN warm"
            );
        }
        _ => {}
    }

    let attempt_id = ann.next_warm_attempt_id.fetch_add(1, Ordering::Relaxed);
    states.insert(
        key.clone(),
        AnnWarmState::Warming {
            attempt_id,
            generation,
            started_at: std::time::Instant::now(),
        },
    );
    Some(AnnWarmPermit {
        ann: ann.clone(),
        key,
        attempt_id,
        generation,
        finished: false,
    })
}

/// Apply the terminal transition only when `permit` still owns the slot.
/// Namespace invalidation or a newer retry makes an older completion a no-op.
pub(super) fn finish_warm_state(permit: &mut AnnWarmPermit, next: AnnWarmState) {
    let mut states = warm_states_guard(&permit.ann.warm_states);
    let started_at = match states.get(&permit.key) {
        Some(AnnWarmState::Warming {
            attempt_id,
            generation,
            started_at,
        }) if *attempt_id == permit.attempt_id && *generation == permit.generation => *started_at,
        _ => {
            permit.finished = true;
            return;
        }
    };

    tracing::debug!(
        key = ?permit.key,
        attempt_id = permit.attempt_id,
        elapsed_ms = started_at.elapsed().as_millis(),
        state = ?next,
        "knowledge ANN warm finished"
    );
    states.insert(permit.key.clone(), next);
    permit.finished = true;
}

/// Finish a normally-returning warm from the worker's explicit outcome. A
/// `Ready` outcome is still verified against the attempt's generation before
/// publication into the lifecycle state.
///
/// The Ready decision and its publication into `warm_states` share one
/// `indexes` read-lock critical section (issue #2340): the guard acquired
/// below stays held across the `warm_states` transition, so a concurrent
/// rotation eviction — which takes `indexes` write then `warm_states` under
/// `evict_bridge_and_ready_state` — cannot run between the decision and the
/// publish. Either the eviction's write-lock acquisition blocks until this
/// guard drops (so it sees the just-published Ready state and can clean it
/// up), or the eviction has already completed and removed the bridge before
/// this guard is taken (so the decision below observes its absence and
/// publishes Failed instead of a dangling Ready).
pub(super) async fn finish_warm(mut permit: AnnWarmPermit, outcome: AnnWarmOutcome) {
    match outcome {
        AnnWarmOutcome::Ready => {
            let ann = permit.ann.clone();
            let idxs = ann.indexes.read().await;
            let next = match idxs
                .get(&permit.key)
                .map(|bridge| bridge.generation)
                .filter(|generation| *generation >= permit.generation)
            {
                Some(generation) => AnnWarmState::Ready { generation },
                None => AnnWarmState::Failed {
                    generation: permit.generation,
                    error: AnnWarmFailure::Operational,
                },
            };
            #[cfg(test)]
            run_finish_warm_ready_test_hook(&ann, &permit.key).await;
            finish_warm_state(&mut permit, next);
            drop(idxs);
        }
        AnnWarmOutcome::Empty => {
            let next = AnnWarmState::Failed {
                generation: permit.generation,
                error: AnnWarmFailure::EmptyCorpus,
            };
            finish_warm_state(&mut permit, next);
        }
        AnnWarmOutcome::Failed => {
            let next = AnnWarmState::Failed {
                generation: permit.generation,
                error: AnnWarmFailure::Operational,
            };
            finish_warm_state(&mut permit, next);
        }
        AnnWarmOutcome::Declined => {
            let next = AnnWarmState::Failed {
                generation: permit.generation,
                error: AnnWarmFailure::NotWarmHost,
            };
            finish_warm_state(&mut permit, next);
        }
    }
}

/// Test-only seam (issue #2340): if a hook is installed on `ann`, spawn the
/// eviction helper against `key` and hand its `JoinHandle` back to the test,
/// all while the caller (`finish_warm`) still holds its `indexes` read
/// guard. Lets a test drive the exact interleaving the fix closes: the
/// spawned eviction task cannot acquire the `indexes` write lock until
/// `finish_warm` finishes publishing and drops its guard.
#[cfg(test)]
pub(super) async fn run_finish_warm_ready_test_hook(ann: &SharedAnn, key: &AnnKey) {
    let hook = ann
        .test_finish_warm_ready_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(hook) = hook else {
        return;
    };
    let ann = ann.clone();
    let key = key.clone();
    let incumbent_digest = hook.incumbent_digest;
    let generation = hook.generation;
    let handle = tokio::spawn(async move {
        evict_bridge_and_ready_state(&ann, &key, incumbent_digest, generation).await;
    });
    let _ = hook.handle_tx.send(handle);
    tokio::task::yield_now().await;
}

impl Drop for AnnWarmPermit {
    fn drop(&mut self) {
        if !self.finished {
            let next = AnnWarmState::Failed {
                generation: self.generation,
                error: AnnWarmFailure::Interrupted,
            };
            finish_warm_state(self, next);
        }
    }
}

#[cfg(test)]
impl AnnWarmPermit {
    /// Leave the state in `Warming` for deterministic cold-start tests.
    pub(super) fn leave_in_flight_for_test(mut self) {
        self.finished = true;
    }
}

// Recover a poisoned unavailable Mutex rather than aborting: the guarded
// HashMap<AnnKey, u64> stays logically valid through a poison (worst case a
// stale reader misses one mark/clear, which only costs an extra wait or an
// extra rebuild attempt — never a wrong terminal result).
pub(super) fn unavailable_guard(
    m: &std::sync::Mutex<HashMap<AnnKey, u64>>,
) -> std::sync::MutexGuard<'_, HashMap<AnnKey, u64>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record that `key`'s corpus scan at `generation` completed and found an
/// empty corpus. Callers must not pass error outcomes here — see the
/// `unavailable` field doc on `AnnState` for the generation-fencing
/// invariant `wait_ready` relies on and why errors never mark.
pub(super) fn mark_unavailable(ann: &SharedAnn, key: &AnnKey, generation: u64) {
    unavailable_guard(&ann.unavailable).insert(key.clone(), generation);
}

/// Returns `true` when `key` has an unavailable marker whose generation is
/// still current, i.e. no corpus mutation has happened since the scan that
/// produced it — nothing will ever populate `indexes` for it, so waiting out
/// the full poll timeout is pointless. A marker that has fallen behind the
/// namespace's current generation is stale and is discarded here so a fresh
/// warm attempt (triggered by the mutation) gets a chance to run.
pub(super) fn is_terminally_unavailable(ann: &SharedAnn, key: &AnnKey) -> bool {
    let current = current_generation(ann, &key.namespace);
    let mut guard = unavailable_guard(&ann.unavailable);
    match guard.get(key) {
        Some(&marked_generation) if marked_generation >= current => true,
        Some(_) => {
            guard.remove(key);
            false
        }
        None => false,
    }
}

/// Insert `bridge` under `key` only if the slot is empty. Returns `true` when
/// the bridge was inserted, `false` if the key was already present.
///
/// Test-only: unlike `install_if_fresher`, this performs no generation
/// fencing at all, so production install sites must never use it.
#[cfg(test)]
pub(crate) async fn insert_ann_if_absent(ann: &SharedAnn, key: AnnKey, bridge: AnnBridge) -> bool {
    use std::collections::hash_map::Entry;
    let mut guard = ann.indexes.write().await;
    match guard.entry(key) {
        Entry::Occupied(_) => false,
        Entry::Vacant(e) => {
            e.insert(bridge);
            true
        }
    }
}

/// Remove all in-memory ANN slots and warm states for `namespace`.
///
/// Called after any corpus mutation so the next search triggers a fresh load.
pub(crate) async fn clear_namespace(ann: &SharedAnn, namespace: &str) {
    // Evict, retire warm ownership, and bump the generation counter while
    // holding both state locks. `begin_warm` serializes on `warm_states`, and
    // `install_if_fresher` serializes on `indexes`, so a post-invalidation
    // attempt cannot be accidentally removed and a pre-invalidation build
    // cannot self-approve into the emptied slot.
    let mut idxs = ann.indexes.write().await;
    let mut states = warm_states_guard(&ann.warm_states);
    idxs.retain(|k, _| k.namespace != namespace);
    states.retain(|k, _| k.namespace != namespace);
    bump_generation(ann, namespace);
}

/// Search the already-loaded index for `key`. Returns `None` on cache miss.
#[cfg(test)]
pub(crate) async fn search_loaded(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Option<Vec<(Uuid, f32)>> {
    let guard = ann.indexes.read().await;
    guard.get(key).map(|bridge| bridge.search(query, k))
}

/// Search the loaded bridge and capture the write-log watermark represented by
/// those candidates under the same read-lock guard. A concurrent checkpoint
/// therefore cannot pair one bridge's hits with another bridge's watermark.
pub(crate) async fn search_loaded_with_seq(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Option<(Vec<(Uuid, f32)>, u64)> {
    if !ann.indexes.read().await.contains_key(key) {
        return None;
    }
    let ann = Arc::clone(ann);
    let key = key.clone();
    let query = query.to_vec();
    tokio::task::spawn_blocking(move || {
        let guard = ann.indexes.blocking_read();
        guard.get(&key).map(|bridge| {
            (
                bridge.search(&query, k),
                bridge.index.last_applied_seq().unwrap_or(0),
            )
        })
    })
    .await
    .expect("loaded ANN traversal task panicked")
}

/// Returns `true` when `key` has a current-generation `Warming` owner but its
/// index has not yet been inserted — i.e. a load is in flight right now.
///
/// `false` means either (a) the index is already loaded, or (b) no warm has
/// been triggered for this key at all (e.g. the corpus is empty).
pub(crate) fn is_warming_not_loaded(ann: &SharedAnn, key: &AnnKey) -> bool {
    let in_warming = {
        let states = warm_states_guard(&ann.warm_states);
        let generation = current_generation(ann, &key.namespace);
        matches!(
            states.get(key),
            Some(AnnWarmState::Warming {
                generation: state_generation,
                ..
            }) if *state_generation >= generation
        )
    };
    if !in_warming {
        return false;
    }
    // Sync check: if index is present, warming finished already.
    // `try_read()` avoids blocking — if the write lock is held we conservatively
    // report warming=true (the write lock is held during insert, so the index is
    // about to appear; treating it as "still warming" is safe).
    match ann.indexes.try_read() {
        Ok(guard) => !guard.contains_key(key),
        Err(_) => true,
    }
}

/// Poll `ann` until `key` appears in the loaded index set, `timeout_ms`
/// elapses, or the warm outcome is discovered to be terminal (issue #1026:
/// an empty or unbuildable corpus can never populate the index, so polling
/// out the full timeout on every query wastes `timeout_ms` for nothing).
///
/// Returns `true` if the index became available within the timeout.
pub(crate) async fn wait_ready(
    ann: &SharedAnn,
    key: &AnnKey,
    timeout_ms: u64,
    poll_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if ann.indexes.read().await.contains_key(key) {
            return true;
        }
        if is_terminally_unavailable(ann, key) {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}

/// Bounded wait for a background ANN warm to complete before a search degrades
/// to FTS-only results. A valid-snapshot cold load on a large corpus can exceed
/// the previous 3s; 5s covers the snapshot deserialize while still bounding the
/// first post-restart query. On timeout the search degrades to FTS-only — it
/// never errors (issue #322).
pub(crate) const ANN_WARM_WAIT_TIMEOUT_MS: u64 = 5_000;
pub(crate) const ANN_WARM_WAIT_POLL_MS: u64 = 50;

/// File-generation polling cadence for mmap bridges. Unchanged ticks read
/// only the small v2 commit record; segment files are reopened only after a
/// distinct publication identity appears.
pub(super) const ROTATION_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

// ── Test-only seam: override the ANN warm-wait timeout ───────────────────────
//
// Zero means use the production default (ANN_WARM_WAIT_TIMEOUT_MS).
// Tests set this to a small value (e.g. 50 ms) to avoid blocking the test
// suite while still exercising the full degrade code path.
pub(super) static ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Returns the effective ANN warm-wait timeout in milliseconds.
///
/// In production this always equals `ANN_WARM_WAIT_TIMEOUT_MS`.  During
/// tests the value may be overridden via `set_warm_wait_timeout_override_ms`
/// to avoid a 5-second stall per test run.
pub(crate) fn warm_wait_timeout_ms() -> u64 {
    let o = ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed);
    if o > 0 {
        o
    } else {
        ANN_WARM_WAIT_TIMEOUT_MS
    }
}

/// Set the ANN warm-wait timeout override for tests.  Pass `0` to restore the
/// production default (`ANN_WARM_WAIT_TIMEOUT_MS`).
#[cfg(test)]
pub(crate) fn set_warm_wait_timeout_override_ms(ms: u64) {
    ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
}
