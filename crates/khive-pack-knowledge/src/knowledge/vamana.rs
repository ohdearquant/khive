// FILE SIZE JUSTIFICATION: This module exceeds the 700-line soft target because it owns
// the complete Vamana ANN lifecycle for knowledge search: SharedAnn type, AnnKey, snapshot
// persistence (warm_known_snapshots / ensure_ann_background), index build (build_ann),
// search (search_loaded_with_seq plus the fresh-tail exact leg), and all associated SQL
// queries and serialization logic. These responsibilities are tightly coupled through the
// shared AnnState and its generation-fenced warm/install protocol; splitting them would obscure
// the lock ordering and ownership contract.

//! Vamana ANN bridge — parallel semantic signal for `knowledge.search`.
//!
//! Wraps `khive_vamana::VamanaIndex` with an ID map (u32 → UUID) so search
//! results can be fused with FTS5 candidates via RRF. Persistence (ADR-079,
//! Amendment 1): v2 binary segments under `<db-file>.ann/<hex>/`, restored
//! through the write-log restart classifier, falling back to legacy v1
//! JSON snapshot rows, then a full corpus rebuild on cache-miss. See
//! crates/khive-pack-knowledge/docs/api/vamana.md for the persistence
//! fallback chain and the file-size/module-coupling rationale.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use khive_retrieval::ann::corpus::{CorpusScope, WatermarkCapture};
use khive_retrieval::ann::registry::{self as ann_registry, WatermarkAuthority};
use khive_runtime::config::ann_rebuild_threshold_from_env as ann_rebuild_threshold;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_vamana::bridge::AnnBridgeCore;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{
    read_commit_info, segment_commit_digest, CorpusFingerprint, VamanaIndex, VamanaSnapshot,
};
use tokio::sync::RwLock;
use uuid::Uuid;

// Reached by the test modules through `use super::*`.
#[cfg(test)]
use khive_vamana::{write_external_ids_sidecar, VamanaConfig};

pub(crate) struct AnnBridge {
    /// The index, its id map and its commit digest; they read as fields of the
    /// bridge through `Deref`.
    core: AnnBridgeCore,
    /// Namespace write-generation this build's corpus scan started at or after
    /// (issue #770). Stamped just before install; `install_if_fresher` uses it
    /// to reject a late-arriving build whose scan predates a `clear_namespace`
    /// invalidation that landed while it was still running.
    generation: u64,
    /// Test-only proof that replacing this bridge drops its mmap owner.
    #[cfg(test)]
    drop_probe: Option<Arc<()>>,
    #[cfg(test)]
    search_pause: Option<Arc<TestSearchPause>>,
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

#[cfg(test)]
#[derive(Default)]
struct TestSearchPause {
    started: AtomicBool,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}

#[cfg(test)]
impl TestSearchPause {
    fn wait(&self) {
        let mut released = self.released.lock().expect("search pause");
        self.started.store(true, Ordering::SeqCst);
        while !*released {
            released = self.wake.wait(released).expect("search pause");
        }
    }

    fn release(&self) {
        *self.released.lock().expect("search pause") = true;
        self.wake.notify_all();
    }
}

/// Cache key for a per-{namespace, model} ANN index slot.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct AnnKey {
    namespace: String,
    model: String,
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
enum AnnWarmFailure {
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
enum AnnWarmState {
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
struct AnnWarmPermit {
    ann: SharedAnn,
    key: AnnKey,
    attempt_id: u64,
    generation: u64,
    finished: bool,
}

/// Shared ANN state: per-{namespace, model} indexes plus a warm lifecycle that
/// permits at most one load/rebuild attempt per key at a time.
pub(crate) struct AnnState {
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    /// Per-key warm lifecycle. `std::sync::Mutex` keeps `begin_warm` usable by
    /// the fire-and-return query path before it spawns an async task.
    warm_states: std::sync::Mutex<HashMap<AnnKey, AnnWarmState>>,
    /// Monotonic ownership token for warm attempts. Generation alone cannot
    /// distinguish a failed retry from its predecessor at the same generation.
    next_warm_attempt_id: AtomicU64,
    /// Per-namespace write-generation counter (issue #770), keyed by
    /// namespace (not the full `AnnKey`). Bumped by `clear_namespace`;
    /// `install_if_fresher` uses it to reject stale builds. See
    /// crates/khive-pack-knowledge/docs/api/vamana.md#annstategenerations-per-namespace-write-generation-counter-issue-770.
    generations: std::sync::Mutex<HashMap<String, u64>>,
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
    unavailable: std::sync::Mutex<HashMap<AnnKey, u64>>,
    /// Keys whose durable consumer registration disappeared while an index
    /// was serving.  Such a bridge is untrusted because another consumer may
    /// already have compacted part of its tail (ADR-118 registration
    /// precondition).  The marker is installed before re-registration and is
    /// cleared only after an authoritative full-corpus scan publishes at the
    /// current namespace generation.
    force_rebuild: std::sync::Mutex<HashSet<AnnKey>>,
    /// Process-local half of checkpoint publication serialization. File-backed
    /// runtimes additionally take the directory lock for cross-process safety;
    /// pathless runtimes need this lock to linearize raise-then-install.
    checkpoint_locks: std::sync::Mutex<HashMap<AnnKey, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// Idempotence guard for the pack-lifetime file-generation watcher.
    rotation_watch_started: AtomicBool,
    /// Whether this process may build an ANN index from the full corpus and
    /// publish the result. A corpus build is minutes of CPU and a segment
    /// rewrite every other reader on the index root must then absorb, and it
    /// pays for itself only in a process that outlives the request. Serving
    /// processes set this from the daemon role at construction; the admin
    /// reindex path sets it unconditionally, because building is what it was
    /// invoked to do. A process without it serves what is already persisted,
    /// or serves degraded and leaves the build to the daemon.
    builds_corpus_indexes: bool,
    /// Test-only rendezvous for `finish_warm`'s Ready publish (issue #2340
    /// regression coverage) — see `run_finish_warm_ready_test_hook`.
    #[cfg(test)]
    test_finish_warm_ready_hook: std::sync::Mutex<Option<Arc<FinishWarmReadyTestHook>>>,
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

fn force_rebuild_guard(
    m: &std::sync::Mutex<HashSet<AnnKey>>,
) -> std::sync::MutexGuard<'_, HashSet<AnnKey>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn mark_force_rebuild(ann: &SharedAnn, key: &AnnKey) {
    force_rebuild_guard(&ann.force_rebuild).insert(key.clone());
}

fn force_rebuild_required(ann: &SharedAnn, key: &AnnKey) -> bool {
    force_rebuild_guard(&ann.force_rebuild).contains(key)
}

fn clear_force_rebuild_if_current(ann: &SharedAnn, key: &AnnKey, generation: u64) {
    if current_generation(ann, &key.namespace) == generation {
        clear_force_rebuild(ann, key);
    }
}

fn clear_force_rebuild(ann: &SharedAnn, key: &AnnKey) {
    force_rebuild_guard(&ann.force_rebuild).remove(key);
}

fn checkpoint_lock(ann: &SharedAnn, key: &AnnKey) -> Arc<tokio::sync::Mutex<()>> {
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
fn generations_guard(
    m: &std::sync::Mutex<HashMap<String, u64>>,
) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bump `namespace`'s write-generation counter and return the new value
/// (issue #770). Called from `clear_namespace`, the single chokepoint every
/// corpus-mutating write already routes through.
fn bump_generation(ann: &SharedAnn, namespace: &str) -> u64 {
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

async fn has_current_index(ann: &SharedAnn, key: &AnnKey) -> bool {
    let idxs = ann.indexes.read().await;
    let current = current_generation(ann, &key.namespace);
    idxs.get(key)
        .is_some_and(|bridge| bridge.generation >= current)
}

async fn has_current_index_at_watermark(ann: &SharedAnn, key: &AnnKey, watermark: u64) -> bool {
    let idxs = ann.indexes.read().await;
    let current = current_generation(ann, &key.namespace);
    idxs.get(key).is_some_and(|bridge| {
        bridge.generation >= current && bridge.index.last_applied_seq().unwrap_or(0) >= watermark
    })
}

// Recover a poisoned warm-state Mutex rather than aborting: each transition is
// one HashMap replacement, so the previous or next complete state remains safe
// to inspect after a poison.
fn warm_states_guard(
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
fn begin_warm(ann: &SharedAnn, key: AnnKey) -> Option<AnnWarmPermit> {
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
fn finish_warm_state(permit: &mut AnnWarmPermit, next: AnnWarmState) {
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
async fn finish_warm(mut permit: AnnWarmPermit, outcome: AnnWarmOutcome) {
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
async fn run_finish_warm_ready_test_hook(ann: &SharedAnn, key: &AnnKey) {
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
    fn leave_in_flight_for_test(mut self) {
        self.finished = true;
    }
}

// Recover a poisoned unavailable Mutex rather than aborting: the guarded
// HashMap<AnnKey, u64> stays logically valid through a poison (worst case a
// stale reader misses one mark/clear, which only costs an extra wait or an
// extra rebuild attempt — never a wrong terminal result).
fn unavailable_guard(
    m: &std::sync::Mutex<HashMap<AnnKey, u64>>,
) -> std::sync::MutexGuard<'_, HashMap<AnnKey, u64>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record that `key`'s corpus scan at `generation` completed and found an
/// empty corpus. Callers must not pass error outcomes here — see the
/// `unavailable` field doc on `AnnState` for the generation-fencing
/// invariant `wait_ready` relies on and why errors never mark.
fn mark_unavailable(ann: &SharedAnn, key: &AnnKey, generation: u64) {
    unavailable_guard(&ann.unavailable).insert(key.clone(), generation);
}

/// Returns `true` when `key` has an unavailable marker whose generation is
/// still current, i.e. no corpus mutation has happened since the scan that
/// produced it — nothing will ever populate `indexes` for it, so waiting out
/// the full poll timeout is pointless. A marker that has fallen behind the
/// namespace's current generation is stale and is discarded here so a fresh
/// warm attempt (triggered by the mutation) gets a chance to run.
fn is_terminally_unavailable(ann: &SharedAnn, key: &AnnKey) -> bool {
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
const ROTATION_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

// ── Test-only seam: override the ANN warm-wait timeout ───────────────────────
//
// Zero means use the production default (ANN_WARM_WAIT_TIMEOUT_MS).
// Tests set this to a small value (e.g. 50 ms) to avoid blocking the test
// suite while still exercising the full degrade code path.
static ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
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

impl AnnBridge {
    fn from_core(core: AnnBridgeCore) -> Self {
        Self {
            core,
            generation: 0,
            #[cfg(test)]
            drop_probe: None,
            #[cfg(test)]
            search_pause: None,
        }
    }

    pub fn build(vectors: Vec<f32>, dim: usize, id_map: Vec<Uuid>) -> Result<Self, String> {
        let core = AnnBridgeCore::build(vectors, dim, id_map)?;
        Ok(Self::from_core(core))
    }

    /// Stamp this bridge with the namespace write-generation its corpus scan
    /// started at or after (issue #770). Called just before install; see
    /// `install_if_fresher`.
    pub(crate) fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Stamp the ann_write_log watermark this bridge's corpus state reflects
    /// (ADR-079 Amendment 1). Persisted by `save_atomic` into the extended
    /// commit record.
    pub(crate) fn set_applied_seq(&mut self, seq: u64) {
        self.index.set_last_applied_seq(Some(seq));
    }

    /// Ordinal lookup for only the subjects in a coalesced tail. Scan the
    /// id-map once, but allocate and hash at most one entry per tail subject
    /// instead of rebuilding a corpus-sized reverse map for a short replay.
    /// Highest ordinal wins for a repeated uuid: inserts append, so the
    /// latest slot is the live one; earlier slots are tombstoned.
    ///
    /// A tombstoned ordinal has no owner (ADR-079 Amendment 1 id-map
    /// ownership rule) — `id_map` entries for already-tombstoned slots are
    /// stale (tombstoning never clears them) and are excluded here, or a
    /// reused slot's new owner can be tombstoned by a replay op for the
    /// old, already-deleted subject (#1150).
    pub(crate) fn reverse_map_for(
        &self,
        subjects: impl IntoIterator<Item = Uuid>,
    ) -> HashMap<Uuid, u32> {
        let wanted: Vec<Uuid> = subjects.into_iter().collect();
        // One/few-row tails are common: comparing UUID bytes is cheaper
        // than hashing every id-map entry while scanning the corpus.
        let wanted_set = (wanted.len() > 4).then(|| wanted.iter().copied().collect::<HashSet<_>>());
        let mut reverse: HashMap<Uuid, u32> =
            HashMap::with_capacity(wanted.len().min(self.index.live_count()));
        for (ordinal, uuid) in self.id_map.iter().enumerate() {
            let in_tail = match &wanted_set {
                Some(set) => set.contains(uuid),
                None => wanted.contains(uuid),
            };
            if !in_tail || self.index.is_tombstoned(ordinal as u32) {
                continue;
            }
            reverse.insert(*uuid, ordinal as u32);
        }
        reverse
    }

    /// Apply one subject's coalesced final state (ADR-079 Amendment 1):
    /// `Some(embedding)` replays a final upsert (tombstone the mapped old
    /// ordinal, then exactly one insert); `None` replays a final delete
    /// (tombstone if mapped, no-op otherwise). `reverse` is the map from
    /// [`reverse_map_for`](Self::reverse_map_for), kept current across calls.
    ///
    /// A delete whose mapped ordinal has been reassigned by an earlier
    /// upsert in this replay is skipped with a warning, not an error: the
    /// old subject's vector was already tombstoned when the slot was
    /// reused, so there is nothing left to delete. Any other id-map
    /// contradiction returns `Err` — the caller escalates to Cold.
    pub(crate) fn apply_final_op(
        &mut self,
        reverse: &mut HashMap<Uuid, u32>,
        uuid: Uuid,
        op: Option<Vec<f32>>,
    ) -> Result<(), String> {
        match op {
            None => {
                if let Some(&ordinal) = reverse.get(&uuid) {
                    // Fail closed on ownership contradictions: if the
                    // slot's current id-map owner is no longer this
                    // subject (an earlier op in this replay already reused
                    // the slot), skip the tombstone rather than delete
                    // someone else's live vector.
                    if self.id_map.get(ordinal as usize) != Some(&uuid) {
                        tracing::warn!(
                            subject = %uuid,
                            ordinal,
                            "replay delete: ordinal reassigned within batch, skipping tombstone"
                        );
                        reverse.remove(&uuid);
                        return Ok(());
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
                    self.index
                        .tombstone(old)
                        .map_err(|e| format!("replay tombstone({old}): {e}"))?;
                }
                let ordinal = self
                    .index
                    .insert(&embedding)
                    .map_err(|e| format!("replay insert: {e}"))?;
                let slot = ordinal as usize;
                match slot.cmp(&self.id_map.len()) {
                    std::cmp::Ordering::Less => self.id_map[slot] = uuid,
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
        Ok(())
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(Uuid, f32)> {
        #[cfg(test)]
        if let Some(pause) = &self.search_pause {
            pause.wait();
        }
        match self.core.search_hits(query, k) {
            Ok(hits) => hits
                .into_iter()
                .map(|(uuid, dist)| {
                    // L2² → cosine: cos(a,b) = 1 - L2²(a,b)/2 for unit vectors
                    let cosine = 1.0 - dist / 2.0;
                    (uuid, cosine.max(0.0))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "vamana ANN search failed");
                Vec::new()
            }
        }
    }

    pub fn num_vectors(&self) -> usize {
        self.index.num_vectors()
    }

    pub fn from_vamana_snapshot(snapshot: VamanaSnapshot) -> Result<Self, String> {
        let id_map: Vec<Uuid> = snapshot
            .external_ids
            .iter()
            .map(|s| Uuid::parse_str(s).map_err(|e| format!("bad UUID {s}: {e}")))
            .collect::<Result<_, _>>()?;
        let index =
            VamanaIndex::from_snapshot(&snapshot).map_err(|e| format!("snapshot restore: {e}"))?;
        let core = AnnBridgeCore {
            index,
            id_map,
            commit_digest: None,
        };
        Ok(Self::from_core(core))
    }

    /// Save this bridge to `dir` atomically: writes v2 Vamana segments (commits
    /// `metadata.bin`), then the id-map sidecar (`external_ids.bin`,
    /// tmp-then-rename) bound to the blake3 digest of the just-committed record.
    /// Crash-safety invariant: a crash between the two writes leaves the
    /// sidecar's stored digest mismatched against the on-disk commit record, so
    /// the load-time cross-check detects the torn pair and the caller
    /// rebuilds -- ordering alone is not the guarantee, the digest cross-check
    /// is. See crates/khive-pack-knowledge/docs/api/vamana.md#save_atomic.
    #[allow(dead_code)]
    pub fn save_atomic(&self, dir: &std::path::Path) -> Result<(), String> {
        let _publication_lock = acquire_bridge_checkpoint_lock(dir)?;
        self.save_atomic_locked(dir)
    }

    /// Save while the caller holds this directory's bridge-level publication
    /// lock.  Keeping the lock above both the Vamana commit and UUID sidecar
    /// prevents two writers from pairing one commit digest with another
    /// writer's id map.
    fn save_atomic_locked(&self, dir: &std::path::Path) -> Result<(), String> {
        // The core writes the v2 segments (metadata.bin is the commit gate), digests the
        // just-committed record, then writes the id-map sidecar bound to that digest so any
        // segment/sidecar pairing from different saves is self-detecting at load time.
        self.core.save_atomic(dir)?;
        Ok(())
    }

    /// Load a bridge from a segment directory previously written by
    /// [`AnnBridge::save_atomic`].
    ///
    /// Both the Vamana v2 commit record and the id-map sidecar must be present and
    /// self-consistent (sidecar bound to the exact commit-record digest, matching
    /// vector count). Any mismatch returns `Err`; the caller should treat that as a
    /// Cold signal and rebuild from the corpus.
    #[allow(dead_code)]
    pub fn load(dir: &std::path::Path) -> Result<Self, String> {
        // The core requires a v2 commit fingerprint (absent/v1/torn → Cold), raw-loads the
        // committed v2 index (VamanaIndex::load is v2-aware, ADR-079), then cross-checks the
        // external_ids sidecar against the exact commit record and the vector count.
        let (core, _commit_digest) = AnnBridgeCore::load(dir)?;
        Ok(Self::from_core(core))
    }
}

const BRIDGE_LOCK_MESSAGE_PREFIX: &str = "ANN bridge";

fn acquire_bridge_checkpoint_lock(dir: &std::path::Path) -> Result<std::fs::File, String> {
    khive_retrieval::ann::acquire_checkpoint_lock(dir, BRIDGE_LOCK_MESSAGE_PREFIX)
}

async fn acquire_bridge_checkpoint_lock_async(
    dir: std::path::PathBuf,
) -> Result<std::fs::File, String> {
    khive_retrieval::ann::acquire_checkpoint_lock_async(dir, BRIDGE_LOCK_MESSAGE_PREFIX).await
}

// ── persistence helpers ───────────────────────────────────────────────────────

mod segment_key;
pub(crate) use segment_key::snapshot_key;
use segment_key::{ann_segment_dir, ann_segment_dir_from_root, decode_ann_dir_name};

/// Model-key sanitization — must match `khive_runtime::sanitize_key`.
pub(crate) fn sanitize_model_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Persist `bridge` as v2 Vamana segments under `<db-file>.ann/<hex>/`.
///
/// Resolves the segment directory via `ann_segment_dir`. Returns `Ok(())` when the
/// backend is in-memory (no database file) — skipping persistence is not an error.
/// `save_atomic` binds the id-map sidecar to the commit-record digest internally;
/// callers do not need to supply a `CorpusFingerprint`.
#[cfg(test)]
pub(crate) fn persist_ann_v2(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    bridge: &AnnBridge,
) -> Result<(), String> {
    match ann_segment_dir(rt, ns, model) {
        Some(dir) => bridge.save_atomic(&dir),
        None => Ok(()), // in-memory backend — no filesystem, skip silently
    }
}

fn persist_ann_v2_locked(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    bridge: &AnnBridge,
) -> Result<(), String> {
    match ann_segment_dir(rt, ns, model) {
        Some(dir) => bridge.save_atomic_locked(&dir),
        None => Ok(()),
    }
}

/// Stable, scope-bearing consumer identity for the knowledge atom index
/// (ADR-079 Amendment 1): pack name plus the corpus predicate's field value,
/// so the same predicate always maps to the same `ann_consumer_watermark`
/// row across restarts.
const ANN_CONSUMER: &str = "knowledge:knowledge.atom";

/// Durably register this consumer's watermark row as pending (`-2`).
///
/// MUST run before the consumer persists or serves any extended-format
/// segment for the scope: pending blocks pair-wide compaction instead of
/// hiding this consumer from the registry `MIN`, but can be retired with a
/// warning if no first checkpoint ever activates it (ADR-079 Amendment 1 §A
/// step 1 and issue #1479).
#[cfg(test)]
async fn register_consumer(rt: &KhiveRuntime, ns: &str, model: &str) -> Result<(), String> {
    let sql = rt.sql();
    if ann_segment_dir(rt, ns, model).is_none() {
        // In-memory SqlBridge atomic units cannot pin their manual transaction
        // across PoolBackedWriter calls. Pathless consumers have no durable
        // pending lifecycle to retire, so one closed-fence statement is enough.
        let mut writer = sql.writer().await.map_err(|error| error.to_string())?;
        writer
            .execute(ann_registry::pathless_register_pending(
                "knowledge_",
                ANN_CONSUMER,
                ns,
                model,
            ))
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    ann_registry::register_pending(sql.as_ref(), ANN_CONSUMER, ns, model)
        .await
        .map_err(|e| e.to_string())
}

/// Durable sentinel for registry-loss recovery.  `-1` is below every legal
/// sequence watermark, so it both blocks pair-wide compaction (`MIN = -1`)
/// and tells every process to reject its loaded/persisted bridge until one
/// authoritative full-corpus checkpoint raises the row to a normal `S >= 0`.
async fn write_force_rebuild_sentinel_row(rt: &KhiveRuntime, key: &AnnKey) -> Result<(), String> {
    let sql = rt.sql();
    if ann_segment_dir(rt, &key.namespace, &key.model).is_none() {
        let mut writer = sql.writer().await.map_err(|error| error.to_string())?;
        writer
            .execute(ann_registry::pathless_mark_recovering(
                "knowledge_",
                ANN_CONSUMER,
                &key.namespace,
                &key.model,
            ))
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    ann_registry::mark_recovering(sql.as_ref(), ANN_CONSUMER, &key.namespace, &key.model)
        .await
        .map_err(|error| error.to_string())
}

async fn write_force_rebuild_sentinel(rt: &KhiveRuntime, key: &AnnKey) -> Result<(), String> {
    // Serialize the sentinel with the complete bridge+sidecar publication and
    // watermark transition.  A checkpoint that began before registry loss
    // therefore either finishes before `-1` is published or observes `-1`
    // under this same lock and aborts without publishing.
    let _publication_lock = match ann_segment_dir(rt, &key.namespace, &key.model) {
        Some(dir) => Some(acquire_bridge_checkpoint_lock_async(dir).await?),
        None => None,
    };
    // The detector may have waited behind a successful authoritative
    // checkpoint. Revalidate under the publication locks so that a delayed
    // request cannot demote the winner's normal row back to -1.
    if matches!(
        read_own_watermark(rt, &key.namespace, &key.model).await?,
        Some(watermark) if watermark >= 0
    ) {
        return Ok(());
    }
    write_force_rebuild_sentinel_row(rt, key).await
}

/// Establish the cross-process precondition for one authoritative rebuild
/// after consumer-registry loss.  The local marker is deliberately set
/// before the first await; the transactional SQL upsert then publishes the
/// same state to every process before this consumer can be treated as
/// registered again.
async fn prepare_authoritative_rebuild(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> Result<(), String> {
    mark_force_rebuild(ann, key);
    let local_publication_lock = checkpoint_lock(ann, key);
    let _local_publication_guard = local_publication_lock.lock().await;
    write_force_rebuild_sentinel(rt, key).await
}

/// Read this consumer's own registry watermark. `None` means decision-rule-4
/// registry loss; knowledge publishes `-1` before its authoritative rebuild.
async fn read_own_watermark(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
) -> Result<Option<i64>, String> {
    let sql = rt.sql();
    // This statement has always been labelled without a pack prefix.
    ann_registry::read_watermark(sql.as_ref(), "", ANN_CONSUMER, ns, model)
        .await
        .map_err(|e| e.to_string())
}

/// Raise this consumer's registered watermark monotonically after a durable
/// segment commit at `s` (ADR-079 Amendment 1 §A step 2). A crash before this
/// leaves the smaller watermark — under-compacts, never over-compacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckpointAuthority {
    Incremental,
    FullRegistered,
    FullSentinel,
}

/// Capture the durable registry state immediately before a full corpus scan.
/// An absent row is published as the cross-process sentinel rather than a
/// plain registration: the scan is authoritative and can safely clear it,
/// while peers must remain Cold for the entire scan window.
pub(crate) async fn prepare_full_corpus_scan(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> Result<CheckpointAuthority, String> {
    match read_own_watermark(rt, &key.namespace, &key.model).await? {
        Some(watermark) if watermark >= 0 => Ok(CheckpointAuthority::FullRegistered),
        Some(watermark) if watermark == ann_registry::RECOVERING_WATERMARK => {
            mark_force_rebuild(ann, key);
            Ok(CheckpointAuthority::FullSentinel)
        }
        Some(_) | None => {
            // Pending (`-2`) is a bounded first-checkpoint grace state, not
            // the authoritative recovery fence this scan is allowed to
            // clear. Direct full-reindex callers do not pass through the
            // ordinary ensure path, so promote it to durable `-1` here
            // before scanning rather than failing their first publication.
            prepare_authoritative_rebuild(rt, ann, key).await?;
            Ok(CheckpointAuthority::FullSentinel)
        }
    }
}

async fn raise_watermark(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
    authority: CheckpointAuthority,
) -> Result<(), String> {
    let shared_authority = match authority {
        CheckpointAuthority::FullSentinel => WatermarkAuthority::Recovering,
        CheckpointAuthority::Incremental => WatermarkAuthority::Active,
        CheckpointAuthority::FullRegistered => WatermarkAuthority::PendingOrActive,
    };
    let sql = rt.sql();
    let raised = if ann_segment_dir(rt, ns, model).is_none() {
        let watermark = i64::try_from(s)
            .map_err(|_| format!("knowledge ANN watermark {s} exceeds SQLite INTEGER range"))?;
        let mut writer = sql.writer().await.map_err(|error| error.to_string())?;
        writer
            .execute(ann_registry::pathless_raise_watermark(
                "knowledge_",
                ANN_CONSUMER,
                ns,
                model,
                watermark,
                shared_authority,
            ))
            .await
            .map_err(|error| error.to_string())?
            == 1
    } else {
        ann_registry::raise_watermark(sql.as_ref(), ANN_CONSUMER, ns, model, s, shared_authority)
            .await
            .map_err(|e| e.to_string())?
    };
    if !raised {
        return Err(format!(
            "ANN watermark publication fence rejected {authority:?}: affected 0 rows"
        ));
    }
    Ok(())
}

fn knowledge_corpus(ns: &str) -> CorpusScope<'_> {
    CorpusScope {
        namespace: Some(ns),
        record_kind: None,
        field: "knowledge.atom",
        live_join: None,
        watermark_capture: WatermarkCapture::LogHighWater,
    }
}

/// Compact the write log through the pair-wide registry minimum ONLY (ADR-079
/// Amendment 1 §A step 3, universal wildcard-inclusive form). Wildcard rows
/// (`namespace = '*'`) are global-scope consumers whose corpus spans every
/// namespace; their watermark bounds this pair's compaction too. The scalar
/// subquery yields NULL when no consumer has registered, and `seq <= NULL`
/// matches nothing — an unregistered pair never compacts.
async fn compact_log(rt: &KhiveRuntime, ns: &str, model: &str) -> Result<(), String> {
    let sql = rt.sql();
    if ann_segment_dir(rt, ns, model).is_none() {
        let mut writer = sql.writer().await.map_err(|error| error.to_string())?;
        writer
            .execute(ann_registry::pathless_compact_log(
                "knowledge_",
                knowledge_corpus(ns).compaction_scope(),
                model,
            ))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    ann_registry::compact_write_log(sql.as_ref(), knowledge_corpus(ns).compaction_scope(), model)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Whether any tail row exists above `s` for this consumer's scope. A pure
/// `ann_write_log` index probe (`idx_ann_write_log_ns_model_seq`) — never
/// touches the vec0 corpus, which is what keeps Hot classification free of
/// corpus IO (the amendment's rule 5/6 evaluation-order note).
async fn tail_exists(rt: &KhiveRuntime, ns: &str, model: &str, s: u64) -> Result<bool, String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(knowledge_corpus(ns).tail_exists(model, s, "ann_tail_probe"))
        .await
        .map_err(|e| e.to_string())?;
    match rows.first().and_then(|r| r.get("has_tail")) {
        Some(SqlValue::Integer(n)) => Ok(*n != 0),
        other => Err(format!("tail probe: unexpected value {other:?}")),
    }
}

/// Live corpus count and tail count for this consumer's scope, captured in ONE
/// statement so both come from the same SQLite read snapshot (the decision
/// table requires the live count and the tail to describe one state).
async fn scope_counts(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
) -> Result<(u64, u64), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(knowledge_corpus(ns).scope_counts(&table_name, model, s, "ann_scope_counts"))
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

/// Classification needs the exact *decision*, not the full live count when
/// the tail is small. This statement counts log rows and reads at most `cap`
/// scope-matching vec0 rows in the same SQLite snapshot. If the cap is reached,
/// it is a proven lower bound high enough for Stale-tail; otherwise the count
/// is exact (including zero, which must classify Empty). A tail of one needs
/// only a scope-existence probe. Extreme cap arithmetic falls back to an
/// unlimited, exact count inside this same statement.
struct ClassificationScopeCounts {
    live_lower_bound: u64,
    tail: u64,
    live_count_exact: bool,
}

async fn classification_scope_counts(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
    threshold: f64,
) -> Result<ClassificationScopeCounts, String> {
    // For tail > 1, cap >= 2 * tail / threshold guarantees that
    // ceil(threshold * cap) >= tail despite floating-point rounding. A
    // multiplier that cannot fit in SQLite INTEGER instead selects the
    // unbounded exact count. The tail=1 case needs just one live row.
    let multiplier = (2.0 / threshold).ceil();
    let multiplier = if multiplier.is_finite() && multiplier < i64::MAX as f64 {
        SqlValue::Integer(multiplier as i64)
    } else {
        SqlValue::Null
    };
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(knowledge_corpus(ns).classification_scope_counts(
            &table_name,
            model,
            s,
            multiplier,
            "ann_classification_scope_counts",
        ))
        .await
        .map_err(|e| e.to_string())?;
    let row = rows
        .into_iter()
        .next()
        .ok_or("classification_scope_counts returned no row")?;
    let get = |col: &str| match row.get(col) {
        Some(SqlValue::Integer(n)) => u64::try_from(*n).map_err(|_| format!("negative {col}")),
        other => Err(format!(
            "classification_scope_counts {col}: unexpected value {other:?}"
        )),
    };
    let live_lower_bound = get("live")?;
    let tail = get("tail")?;
    let cap = match row.get("cap") {
        Some(SqlValue::Integer(n)) => *n,
        other => return Err(format!("classification_scope_counts cap: {other:?}")),
    };
    Ok(ClassificationScopeCounts {
        live_lower_bound,
        tail,
        live_count_exact: cap < 0 || live_lower_bound < cap as u64,
    })
}

/// Coalesce the scope's tail (rows above `s`) to the final op per subject in
/// ONE aggregate query — SQLite's bare-column-with-MAX guarantee makes `op`
/// the value from each subject's max-seq row. Returns `(subject, is_delete)`
/// pairs plus the new watermark; memory is O(distinct tail subjects), never
/// O(tail rows). Embeddings are read separately, per batch, by
/// [`replay_final_states`].
async fn fetch_final_states(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    s: u64,
) -> Result<(Vec<(Uuid, bool)>, u64), String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT subject_id, op, MAX(seq) AS seq FROM ann_write_log \
                  WHERE namespace = ?1 AND embedding_model = ?2 \
                    AND field = 'knowledge.atom' AND seq > ?3 \
                  GROUP BY subject_id"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_owned()),
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
            ],
            label: Some("ann_fetch_final_states".into()),
        })
        .await
        .map_err(|e| e.to_string())?;

    let mut new_s = s;
    let mut finals: Vec<(Uuid, bool)> = Vec::with_capacity(rows.len());
    for row in &rows {
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
        finals.push((uuid, is_delete));
    }
    Ok((finals, new_s))
}

/// Subjects per streamed replay batch: bounds transient replay memory at
/// O(batch × dimensions) regardless of tail size.
const REPLAY_BATCH: usize = 500;

/// Stream the coalesced final states onto `bridge`. Each final upsert's
/// embedding is point-read by single-key equality — the only constraint
/// shape sqlite-vec plans as a primary-key point lookup rather than a full
/// table scan — and the consumer scope predicate is checked in process on
/// the returned row. Batches apply as they are read, so peak memory is one
/// batch of embeddings, never the whole tail. A final upsert whose source
/// row is missing or out of scope is a contradiction → `Err` (caller
/// escalates to Cold).
async fn replay_final_states(
    rt: &KhiveRuntime,
    bridge: &mut AnnBridge,
    ns: &str,
    model: &str,
    finals: &[(Uuid, bool)],
) -> Result<(), String> {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let point_read_sql = format!(
        "SELECT namespace, embedding_model, field, embedding \
         FROM {table_name} WHERE subject_id = ?1"
    );
    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let mut reverse = bridge.reverse_map_for(finals.iter().map(|(uuid, _)| *uuid));

    for batch in finals.chunks(REPLAY_BATCH) {
        let mut embeddings: HashMap<Uuid, Vec<f32>> = HashMap::new();
        for (uuid, is_delete) in batch {
            if *is_delete {
                continue;
            }
            let rows = reader
                .query_all(SqlStatement {
                    sql: point_read_sql.clone(),
                    params: vec![SqlValue::Text(uuid.to_string())],
                    label: Some("ann_replay_point_read".into()),
                })
                .await
                .map_err(|e| e.to_string())?;
            let Some(row) = rows.first() else {
                return Err(format!(
                    "final upsert for {uuid} has no source row (contradiction → Cold)"
                ));
            };
            let in_scope = matches!(row.get("namespace"), Some(SqlValue::Text(t)) if t == ns)
                && matches!(row.get("embedding_model"), Some(SqlValue::Text(t)) if t == model)
                && matches!(row.get("field"), Some(SqlValue::Text(t)) if t == "knowledge.atom");
            if !in_scope {
                return Err(format!(
                    "final upsert for {uuid}: source row left the consumer scope \
                     (contradiction → Cold)"
                ));
            }
            let Some(SqlValue::Blob(bytes)) = row.get("embedding") else {
                return Err(format!("final upsert for {uuid}: embedding missing on row"));
            };
            // `as_chunks` is unstable on stable; keep `chunks_exact` until it lands.
            #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
            let vec: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            embeddings.insert(*uuid, vec);
        }
        for (uuid, is_delete) in batch {
            let op =
                if *is_delete {
                    None
                } else {
                    Some(embeddings.remove(uuid).ok_or_else(|| {
                        format!("final upsert for {uuid}: embedding lost in batch")
                    })?)
                };
            bridge.apply_final_op(&mut reverse, *uuid, op)?;
        }
    }
    Ok(())
}

// ── ADR-118: fresh-tail exact leg ─────────────────────────────────────────

struct FreshTailSnapshot {
    own_watermark: Option<i64>,
    registry_min: Option<i64>,
    live_count: Option<u64>,
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
}

/// The snapshot statement.  `finals` reduces the selected log suffix to one row
/// per subject (its last operation, at the position of its first appearance)
/// before the vector join, so the join reads one embedding per distinct
/// subject rather than one per raw log row.
fn fresh_tail_snapshot_statement(
    ns: &str,
    model: &str,
    watermark: i64,
    live_threshold: Option<f64>,
) -> SqlStatement {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let (live_cte, selected_order, live_join, live_column) = match live_threshold {
        Some(_) => (
            format!(
                "live AS (\
                   SELECT COUNT(*) AS live_count FROM {table_name} \
                   WHERE namespace = ?1 AND embedding_model = ?2 \
                     AND field = 'knowledge.atom'\
                 ),"
            ),
            "ORDER BY seq DESC \
             LIMIT (SELECT CAST(live_count * ?5 AS INTEGER) + \
                       CASE WHEN CAST(live_count * ?5 AS INTEGER) < live_count * ?5 \
                            THEN 1 ELSE 0 END FROM live)",
            "CROSS JOIN live",
            "live.live_count",
        ),
        None => (String::new(), "ORDER BY seq", "", "NULL"),
    };
    let mut params = vec![
        SqlValue::Text(ns.to_owned()),
        SqlValue::Text(model.to_owned()),
        SqlValue::Integer(watermark),
        SqlValue::Text(ANN_CONSUMER.into()),
    ];
    if let Some(threshold) = live_threshold {
        params.push(SqlValue::Float(threshold));
    }
    SqlStatement {
        sql: format!(
            "WITH \
             registry AS (\
               SELECT MIN(watermark) AS registry_min \
               FROM ann_consumer_watermark \
               WHERE (namespace = ?1 OR namespace = '*') \
                 AND embedding_model = ?2\
             ), \
             own AS (\
               SELECT (SELECT watermark FROM ann_consumer_watermark \
                       WHERE consumer = ?4 AND namespace = ?1 \
                         AND embedding_model = ?2) AS own_watermark\
             ), \
             {live_cte} \
             selected AS (\
               SELECT seq, subject_id, op FROM ann_write_log \
               WHERE namespace = ?1 AND embedding_model = ?2 \
                 AND field = 'knowledge.atom' \
                 AND seq > MAX(\
                   ?3, COALESCE((SELECT registry_min FROM registry), ?3)\
                 ) \
               {selected_order}\
             ), \
             finals AS (\
               SELECT first_seq AS seq, subject_id, op FROM (\
                 SELECT MIN(seq) OVER (PARTITION BY subject_id) AS first_seq, \
                        subject_id, op, \
                        ROW_NUMBER() OVER (\
                          PARTITION BY subject_id ORDER BY seq DESC\
                        ) AS final_rank \
                 FROM selected\
               ) WHERE final_rank = 1\
             ) \
             SELECT finals.seq, finals.subject_id, finals.op, \
                    vectors.namespace AS vector_namespace, \
                    vectors.embedding_model AS vector_model, \
                    vectors.field AS vector_field, \
                    vectors.embedding, registry.registry_min, \
                    own.own_watermark, {live_column} AS live_count \
             FROM registry CROSS JOIN own {live_join} \
             LEFT JOIN finals ON 1 = 1 \
             LEFT JOIN {table_name} AS vectors \
               ON vectors.subject_id = finals.subject_id \
             ORDER BY finals.seq"
        ),
        params,
        label: Some("knowledge_ann_fresh_tail_snapshot".into()),
    }
}

/// Read the registry guard, optional live-count cap, selected log suffix, and
/// every final upsert embedding in one SQLite statement.  A single statement
/// is the snapshot primitive on every backend, including the in-memory
/// pool-backed reader whose separate calls may use separate connections.
async fn fetch_fresh_tail_snapshot(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    watermark: u64,
    live_threshold: Option<f64>,
) -> Result<FreshTailSnapshot, String> {
    let watermark = i64::try_from(watermark)
        .map_err(|_| "fresh-tail watermark exceeds SQLite INTEGER range".to_string())?;
    let statement = fresh_tail_snapshot_statement(ns, model, watermark, live_threshold);

    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|error| error.to_string())?;
    let rows = reader
        .query_all(statement)
        .await
        .map_err(|error| error.to_string())?;

    let first = rows
        .first()
        .ok_or_else(|| "fresh-tail snapshot returned no registry row".to_string())?;
    let read_optional_i64 = |column: &str| -> Result<Option<i64>, String> {
        match first.get(column) {
            Some(SqlValue::Integer(value)) => Ok(Some(*value)),
            Some(SqlValue::Null) | None => Ok(None),
            other => Err(format!("fresh-tail {column}: unexpected value {other:?}")),
        }
    };
    let own_watermark = read_optional_i64("own_watermark")?;
    let registry_min = read_optional_i64("registry_min")?;
    let live_count = match read_optional_i64("live_count")? {
        Some(value) => {
            Some(u64::try_from(value).map_err(|_| "negative fresh-tail live_count".to_string())?)
        }
        None => None,
    };

    type RawVector = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
    );
    let mut finals: Vec<(Uuid, bool, RawVector)> = Vec::new();
    let mut index_by_id: HashMap<Uuid, usize> = HashMap::new();
    for row in &rows {
        let Some(SqlValue::Integer(_)) = row.get("seq") else {
            continue;
        };
        let subject = match row.get("subject_id") {
            Some(SqlValue::Text(value)) => Uuid::parse_str(value)
                .map_err(|error| format!("fresh-tail subject_id {value}: {error}"))?,
            other => return Err(format!("fresh-tail subject_id: unexpected value {other:?}")),
        };
        let is_delete = match row.get("op") {
            Some(SqlValue::Text(value)) if value == "delete" => true,
            Some(SqlValue::Text(value)) if value == "upsert" => false,
            other => return Err(format!("fresh-tail op: unexpected value {other:?}")),
        };
        let raw_vector = (
            match row.get("vector_namespace") {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            },
            match row.get("vector_model") {
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
        match index_by_id.get(&subject) {
            Some(&index) => finals[index] = (subject, is_delete, raw_vector),
            None => {
                index_by_id.insert(subject, finals.len());
                finals.push((subject, is_delete, raw_vector));
            }
        }
    }

    let mut ops = Vec::with_capacity(finals.len());
    for (subject, is_delete, (vector_ns, vector_model, vector_field, embedding)) in finals {
        if is_delete {
            ops.push((subject, None));
            continue;
        }
        let in_scope = vector_ns.as_deref() == Some(ns)
            && vector_model.as_deref() == Some(model)
            && vector_field.as_deref() == Some("knowledge.atom");
        if !in_scope {
            return Err(format!(
                "fresh-tail upsert {subject}: vector row outside consumer scope"
            ));
        }
        let Some(bytes) = embedding else {
            return Err(format!(
                "fresh-tail upsert {subject}: embedding is not a blob"
            ));
        };
        if bytes.len() % std::mem::size_of::<f32>() != 0 {
            return Err(format!(
                "fresh-tail upsert {subject}: malformed embedding byte length {}",
                bytes.len()
            ));
        }
        // `as_chunks` is unstable on stable; keep `chunks_exact` until it lands.
        #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
        let embedding = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        ops.push((subject, Some(embedding)));
    }
    Ok(FreshTailSnapshot {
        own_watermark,
        registry_min,
        live_count,
        ops,
    })
}

fn exact_cosine(query: &[f32], embedding: &[f32]) -> f32 {
    if query.len() != embedding.len() || query.is_empty() {
        return 0.0;
    }
    let mut query = query.to_vec();
    let mut embedding = embedding.to_vec();
    l2_normalize(&mut query);
    l2_normalize(&mut embedding);
    query
        .iter()
        .zip(embedding.iter())
        .map(|(left, right)| left * right)
        .sum::<f32>()
        .max(0.0)
}

pub(crate) fn merge_fresh_tail(
    candidates: Vec<(Uuid, f32)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
) -> Vec<(Uuid, f32)> {
    if ops.is_empty() {
        return candidates;
    }
    let mut deletes = HashSet::new();
    let mut upserts = HashMap::new();
    for (subject, op) in ops {
        match op {
            Some(embedding) => {
                upserts.insert(subject, exact_cosine(query, &embedding));
            }
            None => {
                deletes.insert(subject);
            }
        }
    }
    let mut merged: Vec<(Uuid, f32)> = candidates
        .into_iter()
        .filter(|(subject, _)| !deletes.contains(subject) && !upserts.contains_key(subject))
        .collect();
    merged.extend(upserts);
    merged.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    merged
}

pub(crate) async fn merge_fresh_tail_off_thread(
    candidates: Vec<(Uuid, f32)>,
    query: &[f32],
    ops: Vec<(Uuid, Option<Vec<f32>>)>,
) -> Vec<(Uuid, f32)> {
    if ops.is_empty() {
        return candidates;
    }
    let query = query.to_vec();
    tokio::task::spawn_blocking(move || merge_fresh_tail(candidates, &query, ops))
        .await
        .expect("ANN fresh-tail merge task panicked")
}

pub(crate) enum FreshTailOutcome {
    /// Coalesced final tail operations that are valid against the candidate
    /// list the caller already captured from its serving bridge.
    Ops(Vec<(Uuid, Option<Vec<f32>>)>),
    /// A compaction mismatch forced current-query segment re-resolution. These
    /// candidates already come from one coherent replacement segment plus its
    /// own tail and must replace, never merge with, the caller's stale list.
    Replace {
        candidates: Vec<(Uuid, f32)>,
        /// Whether the replacement ANN source returned fewer than the
        /// requested `k` candidates before its own fresh tail was merged.
        source_exhausted: bool,
    },
    /// The exact leg could not run; retain the caller's existing candidates.
    Skipped,
}

pub(crate) async fn fresh_tail_leg(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    watermark: Option<u64>,
) -> FreshTailOutcome {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    if force_rebuild_required(ann, key) {
        // The first detector already evicted the untrusted bridge and retired
        // its Ready ownership.  Do not invalidate the authoritative warm now
        // in flight on every concurrent query; dropping this query's captured
        // candidates is sufficient until that warm replaces the cache.
        return FreshTailOutcome::Replace {
            candidates: Vec::new(),
            source_exhausted: true,
        };
    }
    if !rt.ann_fresh_tail_enabled() {
        return match read_own_watermark(rt, ns, model).await {
            Ok(Some(watermark)) if watermark >= 0 => FreshTailOutcome::Skipped,
            Ok(_) => force_cold_after_registry_loss(rt, ann, key).await,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    namespace = ns,
                    model,
                    "knowledge ANN registry read failed while fresh-tail was disabled"
                );
                FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                }
            }
        };
    }

    match watermark {
        Some(watermark) => fresh_tail_serving(rt, ann, key, query, k, watermark).await,
        None => fresh_tail_capped(rt, ann, key).await,
    }
}

async fn force_cold_after_registry_loss(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
) -> FreshTailOutcome {
    if rt.is_read_only() {
        // Registry loss normally publishes the cross-process `-1` rebuild
        // fence. A frozen snapshot cannot acquire that authority, so evict
        // only process-local candidates and remain on the FTS path.
        clear_namespace(ann, &key.namespace).await;
        return FreshTailOutcome::Replace {
            candidates: Vec::new(),
            source_exhausted: true,
        };
    }
    // Publish the cross-process fence before yielding or evicting local state.
    // A peer checkpoint therefore cannot compact/publish through the gap while
    // this process is transitioning its loaded bridge to Cold.
    if let Err(error) = prepare_authoritative_rebuild(rt, ann, key).await {
        tracing::warn!(
            error = %error,
            namespace = key.namespace,
            model = key.model,
            "knowledge ANN failed to publish registry-loss rebuild sentinel"
        );
    }
    clear_namespace(ann, &key.namespace).await;
    FreshTailOutcome::Replace {
        candidates: Vec::new(),
        source_exhausted: true,
    }
}

fn ready_snapshot_watermark(snapshot: &FreshTailSnapshot) -> Option<u64> {
    snapshot
        .own_watermark
        .filter(|watermark| *watermark >= 0)
        .and_then(|watermark| u64::try_from(watermark).ok())
}

fn nonnegative_registry_min(snapshot: &FreshTailSnapshot) -> u64 {
    snapshot
        .registry_min
        .filter(|watermark| *watermark >= 0)
        .and_then(|watermark| u64::try_from(watermark).ok())
        .unwrap_or(0)
}

async fn fresh_tail_serving(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    watermark: u64,
) -> FreshTailOutcome {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    let snapshot = match fetch_fresh_tail_snapshot(rt, ns, model, watermark, None).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = ns,
                model,
                "knowledge fresh-tail snapshot failed; dropping stale vector leg"
            );
            return FreshTailOutcome::Replace {
                candidates: Vec::new(),
                source_exhausted: true,
            };
        }
    };
    let Some(_own_watermark) = ready_snapshot_watermark(&snapshot) else {
        return force_cold_after_registry_loss(rt, ann, key).await;
    };

    let registry_min = nonnegative_registry_min(&snapshot);
    if registry_min > watermark {
        // The log may already have been compacted through `registry_min`, so
        // candidates from the bridge at `watermark` cannot be paired with a
        // scan floored at that newer value. Prefer the currently published
        // segment, whose commit watermark must cover the registry minimum.
        let published_watermark = ann_segment_dir(rt, ns, model)
            .and_then(|dir| read_commit_info(&dir).ok().flatten())
            .and_then(|info| info.last_applied_seq)
            .filter(|published| *published >= registry_min);

        match published_watermark {
            Some(published_watermark) => {
                // Retire the stale cache entry now. The query below owns a
                // local replacement candidate set, while the next request's
                // normal warm path re-adopts the published segment.
                clear_namespace(ann, ns).await;
                return fresh_tail_reresolve(rt, ann, key, query, k, published_watermark).await;
            }
            None => {
                // Re-resolution is not possible in this query. Preserve the
                // same-snapshot coverage proof above the registry minimum, but
                // do not mix those operations with candidates from the older
                // bridge: return an exact-only replacement vector source.
                clear_namespace(ann, ns).await;
                return FreshTailOutcome::Replace {
                    candidates: merge_fresh_tail_off_thread(Vec::new(), query, snapshot.ops).await,
                    source_exhausted: true,
                };
            }
        }
    }
    FreshTailOutcome::Ops(snapshot.ops)
}

/// Bound re-resolution when peers publish checkpoints faster than one query can
/// load and validate them. The terminal branch keeps only the exact suffix above
/// the last same-snapshot registry floor, avoiding an unprovable mixture with
/// candidates from a segment behind that floor.
const FRESH_TAIL_RERESOLVE_MAX_ROUNDS: u32 = 3;

/// Load the currently published segment, search it, then validate its watermark
/// against the registry minimum and fetch its own tail in one SQLite snapshot.
/// A peer may advance the minimum between the filesystem load and validation;
/// retry from the newly published segment in that case.
async fn fresh_tail_reresolve(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
    published_watermark: u64,
) -> FreshTailOutcome {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    let mut expected_watermark = published_watermark;
    for round in 1..=FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
        let Some(dir) = ann_segment_dir(rt, ns, model) else {
            tracing::warn!(
                key = ?key,
                namespace = ns,
                model,
                "knowledge fresh-tail published segment disappeared during re-resolution"
            );
            return FreshTailOutcome::Replace {
                candidates: Vec::new(),
                source_exhausted: true,
            };
        };
        let query_for_search = query.to_vec();
        let loaded = tokio::task::spawn_blocking(move || {
            let bridge = AnnBridge::load(&dir)?;
            let loaded_watermark = bridge.index.last_applied_seq();
            let candidates = bridge.search(&query_for_search, k);
            Ok::<_, String>((candidates, loaded_watermark))
        })
        .await;
        let (candidates, loaded_watermark) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    namespace = ns,
                    model,
                    "knowledge fresh-tail published segment load failed"
                );
                return FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                };
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    namespace = ns,
                    model,
                    "knowledge fresh-tail segment worker failed during re-resolution"
                );
                return FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                };
            }
        };
        let loaded_watermark = loaded_watermark.unwrap_or(expected_watermark);
        let source_exhausted = candidates.len() < k;

        let snapshot = match fetch_fresh_tail_snapshot(rt, ns, model, loaded_watermark, None).await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    namespace = ns,
                    model,
                    "knowledge fresh-tail re-resolution snapshot failed"
                );
                return FreshTailOutcome::Replace {
                    candidates: Vec::new(),
                    source_exhausted: true,
                };
            }
        };
        let Some(_own_watermark) = ready_snapshot_watermark(&snapshot) else {
            return force_cold_after_registry_loss(rt, ann, key).await;
        };
        let registry_min = nonnegative_registry_min(&snapshot);

        if registry_min <= loaded_watermark {
            return FreshTailOutcome::Replace {
                candidates: merge_fresh_tail_off_thread(candidates, query, snapshot.ops).await,
                source_exhausted,
            };
        }

        if round == FRESH_TAIL_RERESOLVE_MAX_ROUNDS {
            tracing::warn!(
                key = ?key,
                namespace = ns,
                model,
                rounds = round,
                floor = registry_min,
                "knowledge fresh-tail re-resolution did not converge; using exact-only floored suffix"
            );
            return FreshTailOutcome::Replace {
                candidates: merge_fresh_tail_off_thread(Vec::new(), query, snapshot.ops).await,
                source_exhausted: true,
            };
        }

        expected_watermark = registry_min;
    }
    unreachable!("fresh-tail re-resolution loop returns within its bounded rounds")
}

async fn fresh_tail_capped(rt: &KhiveRuntime, ann: &SharedAnn, key: &AnnKey) -> FreshTailOutcome {
    let snapshot = match fetch_fresh_tail_snapshot(
        rt,
        &key.namespace,
        &key.model,
        0,
        Some(ann_rebuild_threshold()),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = key.namespace,
                model = key.model,
                "knowledge capped fresh-tail fetch failed"
            );
            return FreshTailOutcome::Skipped;
        }
    };
    if ready_snapshot_watermark(&snapshot).is_none() {
        return force_cold_after_registry_loss(rt, ann, key).await;
    }
    debug_assert!(snapshot.live_count.is_some());
    FreshTailOutcome::Ops(snapshot.ops)
}

/// Reconcile a checkpoint after another publisher already advanced the durable
/// row. The loser must adopt the winner (when persisted) rather than overwrite
/// a newer segment or demote a recovered row back to `-1`.
/// `observed_watermark` is the winner's durable lower bound while the caller
/// still holds the bridge publication lock.
async fn adopt_checkpoint_winner(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    generation: u64,
    observed_watermark: u64,
) -> bool {
    let installed = match ann_segment_dir(rt, &key.namespace, &key.model) {
        Some(dir) => match AnnBridge::load(&dir) {
            Ok(bridge) if bridge.index.last_applied_seq().unwrap_or(0) >= observed_watermark => {
                // Any incumbent predates registry-loss recovery. Remove it
                // before installing the winner; the normal generation fence
                // still rejects the winner if a local write raced its scan.
                ann.indexes.write().await.remove(key);
                install_replacing(ann, key, bridge.with_generation(generation)).await
            }
            Ok(bridge) => {
                tracing::warn!(
                    key = ?key,
                    bridge_watermark = bridge.index.last_applied_seq().unwrap_or(0),
                    observed_watermark,
                    "checkpoint winner segment trails its durable watermark"
                );
                ann.indexes.write().await.remove(key);
                false
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    key = ?key,
                    "failed to adopt checkpoint race winner"
                );
                ann.indexes.write().await.remove(key);
                false
            }
        },
        // An in-memory runtime has no cross-process segment. A same-process
        // winner may already be installed in the shared AnnState; otherwise
        // the next warm retries as ordinary registered Cold.
        None => {
            let current = has_current_index_at_watermark(ann, key, observed_watermark).await;
            if !current {
                ann.indexes.write().await.remove(key);
            }
            current
        }
    };
    clear_force_rebuild(ann, key);
    installed
}

/// Persist `bridge` at its applied watermark, reopen and publish the mmap
/// segment, then raise this consumer's registry row and compact through the
/// pair MIN. Publishing before the durable raise makes the ADR-118 mismatch
/// window empty for this process; a crash before the raise merely
/// under-compacts. Registration still precedes persistence (ADR-079 Amendment
/// 1 §A step 1). On persist/reopen failure the Owned bridge is installed.
pub(crate) async fn checkpoint_raise_compact_readopt(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    bridge: AnnBridge,
    generation: u64,
    authority: CheckpointAuthority,
) -> bool {
    let ns = key.namespace.as_str();
    let model = key.model.as_str();
    let local_publication_lock = checkpoint_lock(ann, key);
    let _local_publication_guard = local_publication_lock.lock().await;

    // This lock spans the complete knowledge-level publication: Vamana
    // segments, UUID sidecar, mmap verification, and the durable registry
    // transition.  The sentinel writer takes the same lock, so an ordinary
    // replay checkpoint that predates registry loss cannot publish after -1.
    let publication_lock = match ann_segment_dir(rt, ns, model) {
        Some(dir) => match acquire_bridge_checkpoint_lock_async(dir).await {
            Ok(lock) => Some(lock),
            Err(error) => {
                tracing::warn!(error = %error, "failed to acquire ANN checkpoint lock");
                return false;
            }
        },
        None => None,
    };

    let applied = bridge.index.last_applied_seq().unwrap_or(0);
    let own_watermark = match read_own_watermark(rt, ns, model).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, "ann checkpoint registry read failed");
            return false;
        }
    };
    if authority == CheckpointAuthority::FullSentinel {
        if let Some(observed) = own_watermark.filter(|watermark| *watermark >= 0) {
            tracing::info!(
                key = ?key,
                observed_watermark = observed,
                "authoritative ANN rebuild lost publication race; adopting winner"
            );
            let observed = u64::try_from(observed).unwrap_or(0);
            return adopt_checkpoint_winner(rt, ann, key, generation, observed).await;
        }
    } else if let Some(observed) = own_watermark.filter(|watermark| *watermark >= 0) {
        let observed = u64::try_from(observed).unwrap_or(0);
        if observed > applied {
            tracing::info!(
                key = ?key,
                candidate_watermark = applied,
                observed_watermark = observed,
                "stale ANN checkpoint lost publication race; adopting winner"
            );
            return adopt_checkpoint_winner(rt, ann, key, generation, observed).await;
        }
    }
    let authorized = match authority {
        CheckpointAuthority::FullSentinel => own_watermark == Some(-1),
        CheckpointAuthority::Incremental | CheckpointAuthority::FullRegistered => {
            own_watermark.is_some_and(|watermark| watermark >= 0)
        }
    };
    if !authorized {
        mark_force_rebuild(ann, key);
        if let Err(error) = write_force_rebuild_sentinel_row(rt, key).await {
            tracing::warn!(error = %error, "failed to fence unauthorized ANN checkpoint");
        }
        return false;
    }

    if let Err(e) = persist_ann_v2_locked(rt, ns, model, &bridge) {
        tracing::error!(error = %e, "failed to persist v2 Vamana segment");
        install_replacing(ann, key, bridge.with_generation(generation)).await;
        return false;
    }
    let published = match ann_segment_dir(rt, ns, model) {
        Some(dir) => match AnnBridge::load(&dir) {
            Ok(mmap_bridge) => mmap_bridge,
            Err(e) => {
                tracing::warn!(error = %e, "mmap re-adoption failed; serving Owned build");
                bridge
            }
        },
        None => bridge,
    };
    // File-backed publication must replace the in-process bridge before the
    // durable raise, closing the same-process mismatch window. An in-memory
    // runtime has no cross-process segment or compaction peer, so defer its
    // install until the conditional raise succeeds; a losing concurrent full
    // scan then cannot overwrite the winner before discovering the lost race.
    let file_backed = publication_lock.is_some();
    let mut pending_in_memory = Some(published.with_generation(generation));
    let mut installed = false;
    if file_backed {
        installed = install_replacing(
            ann,
            key,
            pending_in_memory.take().expect("published bridge"),
        )
        .await;
    }

    // The durable segment already covers `applied`, and this process now
    // serves that same state. A failed raise only retains extra log rows.
    if let Err(e) = raise_watermark(rt, ns, model, applied, authority).await {
        tracing::warn!(error = %e, "ann watermark raise failed (under-compacts; safe)");
        match read_own_watermark(rt, ns, model).await {
            Ok(Some(observed)) if observed >= 0 => {
                let observed = u64::try_from(observed).unwrap_or(0);
                if observed > applied {
                    return adopt_checkpoint_winner(rt, ann, key, generation, observed).await;
                }
                // Either our conditional transition committed despite an
                // uncertain client result, or the durable row remains behind
                // this candidate. Both are safe under-compaction states.
                if let Some(published) = pending_in_memory.take() {
                    installed = install_replacing(ann, key, published).await;
                }
                clear_force_rebuild(ann, key);
                return installed || has_current_index_at_watermark(ann, key, observed).await;
            }
            Ok(_) => {}
            Err(read_error) => {
                tracing::warn!(
                    error = %read_error,
                    "failed to reconcile ANN checkpoint race"
                );
            }
        }
        mark_force_rebuild(ann, key);
        if let Err(error) = write_force_rebuild_sentinel_row(rt, key).await {
            tracing::warn!(error = %error, "failed to restore ANN checkpoint fence");
        }
        return false;
    }
    if let Some(published) = pending_in_memory {
        installed = install_replacing(ann, key, published).await;
    }
    if authority != CheckpointAuthority::Incremental {
        clear_force_rebuild_if_current(ann, key, generation);
    }
    drop(publication_lock);
    if let Err(e) = compact_log(rt, ns, model).await {
        tracing::warn!(error = %e, "ann log compaction failed (retries next checkpoint)");
    }
    installed
}

/// Try to load a Vamana snapshot for `namespace`+`model` from `retrieval_snapshots`.
///
/// Returns `Ok(None)` when the table is absent, the row is missing, or
/// deserialization fails — all of which are treated as cache-miss signals.
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
            params: vec![SqlValue::Text(key), SqlValue::Text("vamana".into())],
            label: None,
        })
        .await
        .ok()?;

    let row = rows.into_iter().next()?;
    let blob = match row.get("snapshot")? {
        SqlValue::Blob(b) => b.clone(),
        _ => return None,
    };
    serde_json::from_slice::<VamanaSnapshot>(&blob).ok()
}

/// Get the corpus fingerprint by querying the vector store.
pub(crate) async fn compute_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let info = store.info().await.ok()?;
    Some(CorpusFingerprint {
        vector_count: info.entry_count,
        dimensions: info.dimensions as u32,
    })
}

/// Scan the sqlite-vec corpus for `model` and return raw (un-normalized) flat
/// vectors alongside the ordered UUID id-map.
///
/// Rows are fetched `ORDER BY subject_id` so the mapping is deterministic.
/// Returns `Ok(None)` only when a scan COMPLETED and found nothing: the table
/// is empty or no rows pass the byte-length validity check. Store-opening
/// failures propagate as `Err` — `Ok(None)` feeds the terminal unavailable
/// marker (issue #1026), so an operational error must never masquerade as a
/// verified empty corpus. The caller derives `dims` as
/// `flat.len() / id_map.len()`.
async fn scan_corpus_raw(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<(Vec<f32>, Vec<Uuid>, u64)>, RuntimeError> {
    let store = rt
        .vectors_for_model(token, model)
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    let info = store
        .info()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    let count = info.entry_count;
    let dims = info.dimensions;

    if count == 0 || dims == 0 {
        return Ok(None);
    }

    let ns = token.namespace().as_str().to_owned();
    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");
    let model_str = model.to_owned();

    let sql = rt.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    // The global AUTOINCREMENT high-water evaluates inside the SAME statement
    // — and therefore the same SQLite read snapshot — as the corpus rows.
    // Unlike MAX over retained rows it survives compaction, so an
    // authoritative recovery checkpoint never regresses below the untrusted
    // segment it replaces. Future writes have a strictly larger global seq;
    // rows from sibling scopes below S are irrelevant to this corpus.
    let rows = reader
        .query_all(knowledge_corpus(&ns).corpus_scan(&table_name, &model_str, "vamana_corpus_scan"))
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut id_map: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut flat: Vec<f32> = Vec::with_capacity(rows.len() * dims);
    let scan_watermark = rows
        .first()
        .and_then(|row| match row.get("log_s") {
            Some(SqlValue::Integer(n)) => u64::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0);

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
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }

    if id_map.is_empty() {
        return Ok(None);
    }

    Ok(Some((flat, id_map, scan_watermark)))
}

/// Scan the sqlite-vec table and build a fresh `AnnBridge`.
///
/// Returns `None` when there are no vectors or the model is not configured.
pub(crate) async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
    let Some((flat, id_map, scan_watermark)) = scan_corpus_raw(rt, token, model).await? else {
        return Ok(None);
    };
    let dims = flat.len() / id_map.len();
    AnnBridge::build(flat, dims, id_map)
        .map(|mut bridge| {
            bridge.set_applied_seq(scan_watermark);
            Some(bridge)
        })
        .map_err(RuntimeError::Internal)
}

/// Delete all Vamana snapshots for `namespace` from `retrieval_snapshots`.
///
/// Called after any vector-corpus mutation to guarantee `ensure_ann_for_model` cannot
/// load a snapshot that no longer matches the live corpus.  Best-effort: if
/// the `retrieval_snapshots` table doesn't exist yet, the call is a no-op.
pub(crate) async fn invalidate_snapshot(rt: &KhiveRuntime, namespace: &str) {
    let pattern = format!("{}::vamana::%", khive_types::escape_like_literal(namespace));
    let sql = rt.sql();
    let mut w = match sql.writer().await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open writer for Vamana snapshot invalidation");
            return;
        }
    };
    match w
        .execute(SqlStatement {
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1 ESCAPE '\\'".into(),
            params: vec![SqlValue::Text(pattern)],
            label: Some("invalidate_vamana_snapshot".into()),
        })
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("no such table") => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to invalidate Vamana snapshot");
        }
    }
}

/// Run one already-owned warm attempt to completion.
async fn run_warm_attempt(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    permit: AnnWarmPermit,
) {
    let outcome = ensure_ann_for_model(rt, token, ann, model).await;
    finish_warm(permit, outcome).await;
}

/// Await one single-flight warm for the explicit model. Used by both v1 and
/// v2 startup discovery so they share the same lifecycle while retaining the
/// preload path's existing await-before-next-key timing.
async fn warm_ann_for_model_once(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) {
    if model.is_empty() {
        return;
    }
    let key = AnnKey::new(token.namespace().as_str(), model);
    let Some(permit) = begin_warm(ann, key) else {
        return;
    };
    run_warm_attempt(rt, token, ann, model, permit).await;
}

/// Pre-load Vamana snapshots for all `{ns}::vamana::{model}` keys found in
/// `retrieval_snapshots`.  Called from `KnowledgePack::warm()` before the first
/// search request so in-memory indexes are ready without a first-query spike.
///
/// Each unique namespace+model pair gets its own keyed slot; all snapshots are
/// loaded, not just the first one.
pub(crate) async fn warm_known_snapshots(rt: &KhiveRuntime, ann: &SharedAnn) {
    // v1 legacy pass: warm namespaces recorded in retrieval_snapshots, if that
    // table exists. On a v2-only database it will not, so a query error must fall
    // through to the v2 segment enumeration below rather than abort the warm pass.
    let rows = {
        let sql = rt.sql();
        match sql.reader().await {
            Ok(mut reader) => reader
                .query_all(SqlStatement {
                    sql:
                        "SELECT DISTINCT namespace FROM retrieval_snapshots WHERE namespace LIKE ?1"
                            .into(),
                    params: vec![SqlValue::Text("%::vamana::%".into())],
                    label: None,
                })
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };

    for row in &rows {
        let ns_key = match row.get("namespace") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let Some((ns_str, model)) = ns_key.split_once("::vamana::") else {
            continue;
        };
        if ns_str.is_empty() || model.is_empty() {
            continue;
        }
        let ns = match Namespace::parse(ns_str) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let token = match rt.authorize(ns) {
            Ok(t) => t,
            Err(_) => continue,
        };
        warm_ann_for_model_once(rt, &token, ann, model).await;
    }

    // Enumerate v2 segment directories under this database's own ANN root and
    // warm any keys not already loaded by the v1 DB pass above.
    let ann_root = match rt.backend_ann_root() {
        Some(d) => d,
        None => return,
    };
    let read_dir = match std::fs::read_dir(&ann_root) {
        Ok(rd) => rd,
        Err(_) => return, // no ann/ dir yet — nothing to warm
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let hex = name.to_string_lossy();
        let Some((ns_str, model)) = decode_ann_dir_name(hex.as_ref()) else {
            continue;
        };
        let ns = match Namespace::parse(&ns_str) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let token = match rt.authorize(ns) {
            Ok(t) => t,
            Err(_) => continue,
        };
        warm_ann_for_model_once(rt, &token, ann, &model).await;
    }
}

/// Whether `start_rotation_watcher` has claimed its one-shot guard for `ann`.
#[cfg(test)]
pub(crate) fn rotation_watch_started_for_test(ann: &SharedAnn) -> bool {
    ann.rotation_watch_started.load(Ordering::Acquire)
}

/// Start one pack-lifetime watcher that releases mmap generations after peer
/// checkpoints rotate their files. The task retains only a weak ANN reference
/// between ticks and also exits immediately on daemon shutdown.
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
        "knowledge_ann_rotation_watch",
        khive_retrieval::ann::rotation_watch_loop(ROTATION_WATCH_INTERVAL, shutdown, tick),
    ))
}

/// Poll the commit identities of currently installed mmap bridges once.
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
        let dir = ann_segment_dir_from_root(ann_root, &key.namespace, &key.model);
        match segment_commit_digest(&dir) {
            Ok(Some(observed)) if observed != expected => {
                refresh_rotated_segment(ann, &key, expected, dir).await;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    namespace = %key.namespace,
                    model = %key.model,
                    "knowledge ANN rotation check failed; retaining the installed generation"
                );
            }
        }
    }
}

/// Evict `key`'s bridge if it is still the one identified by
/// `incumbent_digest`, and clear a `Ready` warm state at the same
/// `generation`, under one `indexes` write-lock critical section.
///
/// Keeping both mutations inside one critical section closes the split-lock
/// race with `finish_warm`'s Ready publish (issue #2340): `finish_warm`
/// decides Ready and publishes it to `warm_states` while still holding the
/// `indexes` read guard it used for the decision, so whichever side's
/// critical section runs second observes the other's completed effect —
/// never a half-applied state where `warm_states` says `Ready` and no
/// bridge is installed. Keying the state cleanup on the evicted bridge's own
/// generation (not "any Ready") leaves an unrelated build's state alone.
async fn evict_bridge_and_ready_state(
    ann: &SharedAnn,
    key: &AnnKey,
    incumbent_digest: [u8; 32],
    generation: u64,
) -> bool {
    let mut indexes = ann.indexes.write().await;
    if indexes
        .get(key)
        .is_none_or(|bridge| bridge.commit_digest != Some(incumbent_digest))
    {
        return false;
    }
    indexes.remove(key);
    // Only a Ready state at the evicted bridge's own generation describes it.
    // A Warming state belongs to an in-flight warm whose permit still owns
    // the single-flight slot; removing it would let a second warm start
    // alongside the first, and a Ready state at a different generation
    // belongs to a later build this eviction has nothing to say about.
    let mut states = warm_states_guard(&ann.warm_states);
    if matches!(
        states.get(key),
        Some(AnnWarmState::Ready { generation: state_generation }) if *state_generation == generation
    ) {
        states.remove(key);
    }
    true
}

/// Adopt one changed publication under the same lock order as checkpoint
/// writers: process-local key lock, then cross-process bridge lock. A changed
/// but invalid publication evicts the predecessor because its files have
/// already been unlinked; the next search may retry the ordinary warm path.
async fn refresh_rotated_segment(
    ann: &SharedAnn,
    key: &AnnKey,
    expected: [u8; 32],
    dir: std::path::PathBuf,
) {
    let local_lock = checkpoint_lock(ann, key);
    let _local_guard = local_lock.lock().await;

    let incumbent = ann.indexes.read().await.get(key).map(|bridge| {
        (
            bridge.commit_digest,
            bridge.generation,
            bridge.index.last_applied_seq().unwrap_or(0),
        )
    });
    let Some((Some(incumbent_digest), generation, incumbent_seq)) = incumbent else {
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
                namespace = %key.namespace,
                model = %key.model,
                "knowledge ANN rotation reload could not acquire the publication lock"
            );
            return;
        }
    };
    let observed = match segment_commit_digest(&dir) {
        Ok(Some(digest)) if digest != incumbent_digest => digest,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(
                error = %error,
                namespace = %key.namespace,
                model = %key.model,
                "knowledge ANN rotated commit identity could not be read"
            );
            return;
        }
    };

    let replacement = AnnBridge::load(&dir).and_then(|bridge| {
        if bridge.commit_digest != Some(observed) {
            return Err("commit identity changed during locked rotation reload".to_string());
        }
        let replacement_seq = bridge.index.last_applied_seq().unwrap_or(0);
        if replacement_seq < incumbent_seq {
            return Err(format!(
                "rotated segment watermark {replacement_seq} regressed below installed {incumbent_seq}"
            ));
        }
        Ok(bridge.with_generation(generation))
    });

    match replacement {
        Ok(bridge) => {
            if install_replacing(ann, key, bridge).await {
                tracing::debug!(
                    namespace = %key.namespace,
                    model = %key.model,
                    "knowledge ANN adopted rotated mmap generation and released its predecessor"
                );
            }
        }
        Err(error) => {
            evict_bridge_and_ready_state(ann, key, incumbent_digest, generation).await;
            tracing::warn!(
                error = %error,
                namespace = %key.namespace,
                model = %key.model,
                "knowledge ANN rotated generation failed validation; released predecessor and will rebuild on demand"
            );
        }
    }
}

/// Spawn one per-key background warm and return immediately. Current-generation
/// `Warming`/`Ready` states suppress duplicates; `Failed` remains retryable by
/// the next search.
pub(crate) fn ensure_ann_background(rt: &KhiveRuntime, token: &NamespaceToken, ann: &SharedAnn) {
    // Searches against a frozen snapshot may use FTS, an already-loaded
    // bridge, and the load-only fresh-tail leg, but must not turn a cache miss
    // into consumer registration or checkpoint publication.
    if rt.is_read_only() {
        return;
    }
    let model = rt.default_embedder_name().to_string();
    if model.is_empty() {
        return;
    }
    let ns = token.namespace().as_str().to_owned();
    let key = AnnKey::new(&ns, &model);
    let Some(permit) = begin_warm(ann, key) else {
        return;
    };

    let rt = rt.clone();
    let ann = ann.clone();
    // Preserve the request-minted actor/visibility context (ADR-096). Reauthorizing
    // from the namespace here would silently replace it with runtime defaults.
    let token = token.clone();
    // Deliberately detached cache maintenance: this warm attempt is shared
    // across later requests and must not inherit one caller's cancellation or
    // deadline. Request-owned ANN/search fan-out is scoped at its spawn sites.
    tokio::spawn(async move {
        run_warm_attempt(&rt, &token, &ann, &model, permit).await;
    });
}

/// Outcome of the v2-segment decision table for one `(namespace, model)` scope.
enum SegmentOutcome {
    /// An index was installed (Hot, Stale-tail, or a served Stale-rebuild
    /// segment whose replacement rebuild the caller must still run — those
    /// return Cold instead so the rebuild path fires).
    Installed,
    /// Live corpus is zero: no ANN candidate may be served or replayed
    /// (decision rule 5). Caller records the terminal unavailable marker.
    Empty,
    /// No trustworthy segment: fall through to the v1 / rebuild paths.
    Cold,
    /// Registry loss or its durable marker requires a full corpus scan.  The
    /// caller must bypass both v2 adoption and the legacy-v1 fallback.
    ForceRebuild,
}

/// ADR-079 Amendment 1 restart classifier (the 8-rule first-match decision
/// table), evaluated for one consumer scope, followed by the matching
/// adoption action. Replaces the retired full-corpus content-hash gate.
#[allow(clippy::too_many_arguments)]
async fn classify_and_adopt_segment(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    ns: &str,
    model: &str,
    seg_dir: &std::path::Path,
    target_generation: u64,
) -> SegmentOutcome {
    if force_rebuild_required(ann, key) {
        return SegmentOutcome::ForceRebuild;
    }

    // Rule 1: commit record absent, corrupt, or invalid length → Cold.
    let info = match read_commit_info(seg_dir) {
        Ok(Some(info)) => info,
        Ok(None) => return SegmentOutcome::Cold,
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "error reading v2 commit record; Cold");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 2: readable but pre-amendment (no watermark) → Cold. Compaction
    // stays blocked naturally: this consumer's pending (`-2`) or recovery
    // (`-1`) row holds the pair MIN below every log sequence.
    let Some(s) = info.last_applied_seq else {
        tracing::info!(namespace = %ns, model = %model,
            "pre-amendment v2 segment (no watermark); Cold rebuild");
        return SegmentOutcome::Cold;
    };

    // Rule 3: configured embedder dimensions ≠ segment dimensions → Cold.
    // Resolved from the embedder registry — no storage access. The corpus
    // is normally touched by one bounded statement after the tail probe:
    // `classification_scope_counts` below (with an exact-query fallback).
    match rt.embedder_dimensions(model) {
        Some(dims) if dims as u64 == info.dimensions => {}
        Some(dims) => {
            tracing::info!(namespace = %ns, model = %model,
                segment_dims = info.dimensions, live_dims = dims,
                "v2 segment dimension mismatch; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        None => return SegmentOutcome::Cold,
    }

    // Rule 4: an absent row or the durable -1 sentinel requires an
    // authoritative full-corpus rebuild.  The sentinel is cross-process and
    // keeps pair compaction blocked until that rebuild publishes.
    match read_own_watermark(rt, ns, model).await {
        Ok(Some(watermark)) if watermark >= 0 => {}
        Ok(Some(_)) => {
            mark_force_rebuild(ann, key);
            return SegmentOutcome::ForceRebuild;
        }
        Ok(None) => {
            tracing::info!(namespace = %ns, model = %model,
                "ann consumer registry row absent; fencing for authoritative rebuild");
            if let Err(error) = prepare_authoritative_rebuild(rt, ann, key).await {
                tracing::warn!(error = %error, "ann consumer re-registration failed");
            }
            return SegmentOutcome::ForceRebuild;
        }
        Err(e) => {
            tracing::warn!(error = %e, "ann registry read failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // Rule 6, tested first per the amendment's evaluation-order note: the
    // tail predicate is a log-table-only index probe, so the Hot path never
    // touches the vec0 corpus at all. With an empty tail the committed
    // segment already reflects every logged op at or below S, so a zero-live
    // scope implies an empty segment and adoption serves exactly what Empty
    // serves.
    match tail_exists(rt, ns, model, s).await {
        Ok(false) => {
            return match AnnBridge::load(seg_dir) {
                Ok(bridge) => {
                    install_if_fresher(ann, key, bridge.with_generation(target_generation)).await;
                    SegmentOutcome::Installed
                }
                Err(e) => {
                    tracing::warn!(error = %e, dir = %seg_dir.display(),
                        "Hot segment load failed; Cold rebuild");
                    SegmentOutcome::Cold
                }
            };
        }
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, "ann tail probe failed; Cold");
            return SegmentOutcome::Cold;
        }
    }

    // A tail exists. Rules 5, 7, and 8 use one snapshot; a short tail
    // usually needs only a bounded scope count, not a full vec0 scan.
    let rebuild_fraction = ann_rebuild_threshold();
    let counts = match classification_scope_counts(rt, ns, model, s, rebuild_fraction).await {
        Ok(counts) => counts,
        Err(error) => {
            tracing::warn!(error = %error, "bounded ANN scope count failed; retrying exact count");
            match scope_counts(rt, ns, model, s).await {
                Ok((live, tail)) => ClassificationScopeCounts {
                    live_lower_bound: live,
                    tail,
                    live_count_exact: true,
                },
                Err(e) => {
                    tracing::warn!(error = %e, "ann scope-count read failed; Cold");
                    return SegmentOutcome::Cold;
                }
            }
        }
    };
    let mut live = counts.live_lower_bound;
    let mut tail = counts.tail;

    let mut threshold = (rebuild_fraction * live as f64).ceil() as u64;
    // A saturated cap must prove Stale-tail. This fallback keeps the exact
    // decision if floating-point or SQLite arithmetic ever violates that
    // conservative bound; its fresh exact query is internally one snapshot.
    if !counts.live_count_exact && tail > threshold {
        match scope_counts(rt, ns, model, s).await {
            Ok((exact_live, exact_tail)) => {
                live = exact_live;
                tail = exact_tail;
                threshold = (rebuild_fraction * live as f64).ceil() as u64;
            }
            Err(e) => {
                tracing::warn!(error = %e, "ANN exact scope-count fallback failed; Cold");
                return SegmentOutcome::Cold;
            }
        }
    }

    // Rule 5: zero live corpus → Empty, regardless of tail contents.
    if live == 0 {
        tracing::info!(namespace = %ns, model = %model,
            "zero live corpus for scope; Empty (FTS/degraded path)");
        return SegmentOutcome::Empty;
    }

    // Rule 7: tail within threshold → Stale-tail: mmap load + final-state
    // replay, then checkpoint so the next restart's tail starts empty and the
    // served bridge returns to mmap backing.
    if tail <= threshold {
        let mut bridge = match AnnBridge::load(seg_dir) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, dir = %seg_dir.display(),
                    "Stale-tail segment load failed; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        let (finals, new_s) = match fetch_final_states(rt, ns, model, s).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "tail replay contradiction; Cold rebuild");
                return SegmentOutcome::Cold;
            }
        };
        if let Err(e) = replay_final_states(rt, &mut bridge, ns, model, &finals).await {
            tracing::warn!(error = %e, "tail replay failed; Cold rebuild");
            return SegmentOutcome::Cold;
        }
        bridge.set_applied_seq(new_s);
        // Replay is cheap and in memory; the checkpoint that follows it is a full
        // segment publication. A process that is not the warm index host serves the
        // replayed bridge and publishes nothing — same answers, no rewrite for every
        // other reader on the root to absorb.
        if !ann.builds_corpus_indexes {
            install_if_fresher(ann, key, bridge.with_generation(target_generation)).await;
            return SegmentOutcome::Installed;
        }
        let checkpointed = checkpoint_raise_compact_readopt(
            rt,
            ann,
            key,
            bridge,
            target_generation,
            CheckpointAuthority::Incremental,
        )
        .await;
        if !checkpointed && force_rebuild_required(ann, key) {
            return SegmentOutcome::ForceRebuild;
        }
        return SegmentOutcome::Installed;
    }

    // Rule 8: tail above threshold → Stale-rebuild: serve the checksum-valid
    // segment while the caller's rebuild path replaces it (`install_replacing`
    // on completion). Cost decision, never a demotion to Cold/FTS-only.
    match AnnBridge::load(seg_dir) {
        Ok(bridge) => {
            tracing::info!(namespace = %ns, model = %model, tail, live,
                "tail above rebuild threshold; serving stale segment during rebuild");
            install_if_fresher(ann, key, bridge.with_generation(target_generation)).await;
            // Rule 8 serves stale and lets the caller rebuild. Only the warm index
            // host has a rebuild to fall through to; for anyone else the stale
            // segment IS the answer for this request.
            if !ann.builds_corpus_indexes {
                return SegmentOutcome::Installed;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, dir = %seg_dir.display(),
                "Stale-rebuild segment load failed; rebuilding without serve-stale");
        }
    }
    SegmentOutcome::Cold
}

/// Lazy warm-load for a specific `model`. Load order (first hit wins): (1)
/// in-memory cache fast path, (2) v2 segment directory (ADR-079 Amendment 1
/// write-log restart classifier — see `classify_and_adopt_segment`), (3)
/// legacy v1 JSON snapshot, (4) full corpus rebuild, atomically persisted
/// as v2 for next restart. See
/// crates/khive-pack-knowledge/docs/api/vamana.md#ensure_ann_for_model-load-order
/// for the per-step detail. The explicit outcome lets the lifecycle retain a
/// served stale fallback while keeping a failed replacement retryable.
pub(crate) async fn ensure_ann_for_model(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> AnnWarmOutcome {
    if model.is_empty() {
        return AnnWarmOutcome::Empty;
    }
    let ns = token.namespace().as_str().to_owned();
    let key = AnnKey::new(&ns, model);

    // A corpus-scale build is minutes of CPU and a segment rewrite every other
    // reader on the root must then take. It pays for itself only in a process
    // that outlives the request, so only the warm daemon does it; everyone else
    // serves what is already persisted, or serves degraded and lets the daemon
    // build. The same answer gates the registry writes below: publishing a
    // rebuild sentinel is an authority act, and a process that will not do the
    // rebuild must not claim it.
    let warm_host = ann.builds_corpus_indexes;

    // Registration precedes every scan or legacy serve. Local absence of a
    // bridge cannot prove global first use: a peer may still hold stale v1 or
    // Owned state after this row was administratively removed. Every absent
    // knowledge row therefore publishes the durable force-rebuild sentinel
    // before any further serving, even in a fresh process.
    let mut force_rebuild = force_rebuild_required(ann, &key);
    match read_own_watermark(rt, &ns, model).await {
        Ok(Some(watermark)) if watermark < 0 => {
            if !warm_host {
                tracing::debug!(namespace = %ns, model = %model,
                    "ANN rebuild is pending and this process is not the warm index host; \
                     declining");
                return AnnWarmOutcome::Declined;
            }
            mark_force_rebuild(ann, &key);
            force_rebuild = true;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            if !warm_host {
                tracing::debug!(namespace = %ns, model = %model,
                    "ANN registry row absent and this process is not the warm index host; \
                     declining rather than claiming the rebuild");
                return AnnWarmOutcome::Declined;
            }
            if let Err(error) = prepare_authoritative_rebuild(rt, ann, &key).await {
                tracing::warn!(error = %error, "failed to fence ANN registry loss");
                return AnnWarmOutcome::Failed;
            }
            force_rebuild = true;
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to read ANN registration before scan");
            return AnnWarmOutcome::Failed;
        }
    }
    if force_rebuild && !matches!(read_own_watermark(rt, &ns, model).await, Ok(Some(-1))) {
        if let Err(error) = prepare_authoritative_rebuild(rt, ann, &key).await {
            tracing::warn!(error = %error, "failed to establish ANN rebuild sentinel");
            return AnnWarmOutcome::Failed;
        }
    }

    // Capture the namespace's write-generation BEFORE anything else (issue
    // #770) — including before the fast path below and before the corpus
    // scan — so a write that lands after this point is guaranteed to be
    // reflected as a higher generation than anything this build can install.
    let target_generation = current_generation(ann, &ns);

    // 1. Fast path: already loaded AND at least as fresh as this namespace's
    // current generation (PR #815). A present entry with a
    // stale generation is not a hit — mere presence let a pre-invalidation
    // build served from an emptied-then-refilled slot serve indefinitely.
    // Falling through here re-enters the same rebuild path a genuine cache
    // miss would take.
    if !force_rebuild {
        if let Some(loaded_generation) = ann
            .indexes
            .read()
            .await
            .get(&key)
            .map(|bridge| bridge.generation)
        {
            if loaded_generation >= target_generation {
                return AnnWarmOutcome::Ready;
            }
            tracing::debug!(
                namespace = %ns,
                model = %model,
                loaded_generation,
                target_generation,
                "knowledge ANN fast path skipped: cached entry generation stale; rebuilding"
            );
        }
    }

    // 2. v2 segment path — ADR-079 Amendment 1 watermark classifier. Total,
    // first-match decision table over the persisted commit record, this
    // consumer's registry row, and one same-snapshot (live, tail) read.
    if !force_rebuild {
        if let Some(seg_dir) = ann_segment_dir(rt, &ns, model) {
            match classify_and_adopt_segment(rt, ann, &key, &ns, model, &seg_dir, target_generation)
                .await
            {
                SegmentOutcome::Installed => return AnnWarmOutcome::Ready,
                SegmentOutcome::Empty => {
                    mark_unavailable(ann, &key, target_generation);
                    return AnnWarmOutcome::Empty;
                }
                SegmentOutcome::Cold => {} // fall through to v1 / rebuild
                SegmentOutcome::ForceRebuild => force_rebuild = true,
            }
        }
    }

    // 3. v1 JSON snapshot path (backwards-compat transition).
    if !force_rebuild {
        if let Some(snapshot) = try_load_snapshot(rt, &ns, model).await {
            let current_fp = compute_fingerprint(rt, token, model).await;
            if let Some(fp) = current_fp {
                if snapshot.fingerprint == fp {
                    match AnnBridge::from_vamana_snapshot(snapshot) {
                        Ok(bridge) => {
                            install_if_fresher(
                                ann,
                                &key,
                                bridge.with_generation(target_generation),
                            )
                            .await;
                            return AnnWarmOutcome::Ready;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "corrupt Vamana v1 snapshot; rebuilding");
                        }
                    }
                } else {
                    tracing::info!(
                        namespace = %ns,
                        model = %model,
                        "stale Vamana v1 snapshot (fingerprint mismatch); rebuilding"
                    );
                }
            }
        }
    }

    // 4. Rebuild fallthrough — build from vector store, persist and re-adopt
    // the v2 segment, then raise the registry watermark and compact the log.
    if !warm_host {
        tracing::info!(namespace = %ns, model = %model,
            "no adoptable ANN segment and this process is not the warm index host; \
             serving degraded and leaving the corpus build to the daemon");
        return AnnWarmOutcome::Declined;
    }
    let scan_authority = match prepare_full_corpus_scan(rt, ann, &key).await {
        Ok(authority) => authority,
        Err(error) => {
            tracing::warn!(error = %error, "failed to establish ANN full-scan authority");
            return AnnWarmOutcome::Failed;
        }
    };
    match load_and_build_from_vector_store(rt, token, model).await {
        Ok(Some(bridge)) => {
            let checkpointed = checkpoint_raise_compact_readopt(
                rt,
                ann,
                &key,
                bridge,
                target_generation,
                scan_authority,
            )
            .await;
            if checkpointed {
                AnnWarmOutcome::Ready
            } else if force_rebuild_required(ann, &key) {
                AnnWarmOutcome::Failed
            } else if has_current_index(ann, &key).await {
                // A normal cold build may still install an Owned bridge when
                // persistence fails, and a lost FullSentinel race may adopt
                // the winner while reporting a fenced local publication.
                AnnWarmOutcome::Ready
            } else {
                AnnWarmOutcome::Failed
            }
        }
        Ok(None) => {
            // Empty corpus: this scan (at target_generation) proves nothing is
            // buildable right now. Mark it so wait_ready can short-circuit
            // instead of polling out the full warm-wait timeout (issue #1026).
            mark_unavailable(ann, &key, target_generation);
            // An authoritative Empty scan keeps the durable -1 sentinel: no
            // segment exists whose publication could advance the row safely.
            // The first subsequent vector write invalidates this terminal
            // marker and the next full scan checkpoints normally.
            AnnWarmOutcome::Empty
        }
        Err(e) => {
            // Operational failure (store open, SQL reader, corpus query) —
            // not proof the corpus is unbuildable. Do NOT mark unavailable:
            // the caller transitions the warm state to retryable `Failed`, and
            // a marker here would make wait_ready short-circuit false while
            // that retry is in flight.
            tracing::warn!(error = %e, "failed to rebuild Vamana ANN index");
            AnnWarmOutcome::Failed
        }
    }
}

/// Simulate an in-flight warm without populating the index. Call this in tests
/// to construct the state that triggers the cold-start guard in
/// `suggest`/`search`.
#[cfg(test)]
pub(crate) fn simulate_warming_in_flight(ann: &SharedAnn, key: AnnKey) {
    begin_warm(ann, key)
        .expect("fresh test ANN state must accept a warm")
        .leave_in_flight_for_test();
}

#[cfg(test)]
#[path = "vamana_owned_build_tests.rs"]
mod owned_build_tests;

#[cfg(test)]
#[path = "vamana_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "vamana_corpus_statement_tests.rs"]
mod corpus_statement_tests;

#[cfg(test)]
#[path = "vamana_corpus_capture_tests.rs"]
mod corpus_capture_tests;
