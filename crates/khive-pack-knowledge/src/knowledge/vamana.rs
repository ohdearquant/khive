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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use khive_runtime::ann_registry::{self, CompactionScope, WatermarkAuthority};
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_vamana::{
    read_commit_fingerprint, read_commit_info, read_external_ids_sidecar, segment_commit_digest,
    write_external_ids_sidecar, CorpusFingerprint, VamanaConfig, VamanaIndex, VamanaSnapshot,
};
use tokio::sync::RwLock;
use uuid::Uuid;

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    id_map: Vec<Uuid>,
    /// Namespace write-generation this build's corpus scan started at or after
    /// (issue #770). Stamped just before install; `install_if_fresher` uses it
    /// to reject a late-arriving build whose scan predates a `clear_namespace`
    /// invalidation that landed while it was still running.
    generation: u64,
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
}

/// Result of one load/rebuild worker. Kept separate from `AnnWarmState` so a
/// failed replacement can remain retryable even when ADR-079 rule 8 left a
/// stale-but-servable bridge installed during the attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnWarmOutcome {
    Ready,
    Empty,
    Failed,
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
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(AnnState {
        indexes: RwLock::new(HashMap::new()),
        warm_states: std::sync::Mutex::new(HashMap::new()),
        next_warm_attempt_id: AtomicU64::new(1),
        generations: std::sync::Mutex::new(HashMap::new()),
        unavailable: std::sync::Mutex::new(HashMap::new()),
        force_rebuild: std::sync::Mutex::new(HashSet::new()),
        checkpoint_locks: std::sync::Mutex::new(HashMap::new()),
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

/// Install `candidate`, replacing an equal-generation incumbent.
///
/// Same namespace-generation fence as `install_if_fresher` (a candidate that
/// predates the namespace's current generation is rejected), but ties REPLACE
/// instead of keeping the incumbent. Only two ordered-within-one-warm-task
/// paths use it: swapping a just-persisted segment's mmap reopen in for the
/// Owned build product (identical content), and replacing a served stale
/// segment with its completed rebuild (rule 8 → rebuild completion). The
/// A/B-race protection that motivates tie-keeps-incumbent in
/// `install_if_fresher` does not apply inside a single single-flight task.
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
async fn finish_warm(mut permit: AnnWarmPermit, outcome: AnnWarmOutcome) {
    let next = match outcome {
        AnnWarmOutcome::Ready => match permit
            .ann
            .indexes
            .read()
            .await
            .get(&permit.key)
            .map(|bridge| bridge.generation)
            .filter(|generation| *generation >= permit.generation)
        {
            Some(generation) => AnnWarmState::Ready { generation },
            None => AnnWarmState::Failed {
                generation: permit.generation,
                error: AnnWarmFailure::Operational,
            },
        },
        AnnWarmOutcome::Empty => AnnWarmState::Failed {
            generation: permit.generation,
            error: AnnWarmFailure::EmptyCorpus,
        },
        AnnWarmOutcome::Failed => AnnWarmState::Failed {
            generation: permit.generation,
            error: AnnWarmFailure::Operational,
        },
    };
    finish_warm_state(&mut permit, next);
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
    let guard = ann.indexes.read().await;
    guard.get(key).map(|bridge| {
        (
            bridge.search(query, k),
            bridge.index.last_applied_seq().unwrap_or(0),
        )
    })
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
    pub fn build(mut vectors: Vec<f32>, dim: usize, id_map: Vec<Uuid>) -> Result<Self, String> {
        if dim == 0 {
            return Err("dimension must be > 0".into());
        }
        if vectors.is_empty() || id_map.is_empty() {
            return Err("no vectors to build ANN index from".into());
        }
        let n = vectors.len() / dim;
        if n != id_map.len() {
            return Err(format!(
                "id_map length {} != vector count {}",
                id_map.len(),
                n
            ));
        }
        // L2→cosine conversion requires unit vectors; normalize before building.
        for row in vectors.chunks_exact_mut(dim) {
            l2_normalize(row);
        }
        let cfg = VamanaConfig::with_dimensions(dim);
        let index = VamanaIndex::build(&vectors, cfg).map_err(|e| format!("{e}"))?;
        Ok(Self {
            index,
            id_map,
            generation: 0,
        })
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

    /// Ordinal lookup for streamed tail replay. Built once per replay so
    /// batches can apply incrementally without rescanning the id-map.
    /// Highest ordinal wins for a repeated uuid: inserts append, so the
    /// latest slot is the live one; earlier slots are tombstoned.
    ///
    /// A tombstoned ordinal has no owner (ADR-079 Amendment 1 id-map
    /// ownership rule) — `id_map` entries for already-tombstoned slots are
    /// stale (tombstoning never clears them) and are excluded here, or a
    /// reused slot's new owner can be tombstoned by a replay op for the
    /// old, already-deleted subject (#1150).
    pub(crate) fn reverse_map(&self) -> HashMap<Uuid, u32> {
        let mut reverse: HashMap<Uuid, u32> = HashMap::with_capacity(self.index.live_count());
        for (ordinal, uuid) in self.id_map.iter().enumerate() {
            if self.index.is_tombstoned(ordinal as u32) {
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
    /// [`reverse_map`](Self::reverse_map), kept current across calls.
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
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        match self.index.search(&q, k) {
            Ok(results) => results
                .into_iter()
                .filter_map(|(idx, dist)| {
                    self.id_map.get(idx as usize).map(|uuid| {
                        // L2² → cosine: cos(a,b) = 1 - L2²(a,b)/2 for unit vectors
                        let cosine = 1.0 - dist / 2.0;
                        (*uuid, cosine.max(0.0))
                    })
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
        Ok(Self {
            index,
            id_map,
            generation: 0,
        })
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
        let count = self.id_map.len();
        if count != self.index.num_vectors() {
            return Err(format!(
                "id_map length {count} != index.num_vectors() {}",
                self.index.num_vectors()
            ));
        }

        // Step 1: write v2 segments atomically (metadata.bin is the commit gate).
        self.index
            .save_atomic(dir)
            .map_err(|e| format!("VamanaIndex::save_atomic: {e}"))?;

        // Step 2: digest the just-committed record. Must be Some — we committed it.
        let digest = segment_commit_digest(dir)
            .map_err(|e| format!("segment_commit_digest after save: {e}"))?
            .ok_or_else(|| {
                "save_atomic succeeded but metadata.bin is absent (torn commit)".to_string()
            })?;

        // Step 3: write the id-map sidecar atomically (tmp rename), bound to the
        // commit-record digest so any segment/sidecar pairing from different
        // saves is self-detecting at load time.
        write_external_ids_sidecar(dir, &digest, &self.id_map).map_err(|e| e.to_string())
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
        // Step 1: require a v2 commit fingerprint. Absent/v1/torn → Cold.
        read_commit_fingerprint(dir)
            .map_err(|e| format!("read_commit_fingerprint: {e}"))?
            .ok_or_else(|| {
                "no v2 commit fingerprint: segment dir is absent, v1, or has a torn commit"
                    .to_string()
            })?;

        // Step 2: raw-load the committed v2 index. VamanaIndex::load is v2-aware
        // (ADR-079): it reads the segments, verifies their checksums, and restores
        // graph + lifecycle without a corpus and without rebuilding. A torn or
        // mismatched segment surfaces as an error, which the caller treats as Cold.
        let index = VamanaIndex::load(dir).map_err(|e| format!("VamanaIndex::load: {e}"))?;

        // Step 3: read the external_ids sidecar and run cross-checks.
        let (sidecar_digest, id_map) = read_external_ids_sidecar(dir)?;

        // Cross-check: the sidecar must be bound to the exact commit record on
        // disk. A mismatch means a segment/sidecar pairing from different saves
        // (crash between the segment commit and the sidecar write, either order).
        let commit_digest = segment_commit_digest(dir)
            .map_err(|e| format!("segment_commit_digest: {e}"))?
            .ok_or_else(|| "metadata.bin vanished between fingerprint and digest".to_string())?;
        if sidecar_digest != commit_digest {
            return Err(
                "external_ids.bin commit-digest mismatch: torn segment/sidecar pair".to_string(),
            );
        }

        // Cross-check: sidecar UUID count must match the loaded index vector count.
        if id_map.len() != index.num_vectors() {
            return Err(format!(
                "external_ids.bin count {} != index.num_vectors() {}",
                id_map.len(),
                index.num_vectors()
            ));
        }

        Ok(Self {
            index,
            id_map,
            generation: 0,
        })
    }
}

fn acquire_bridge_checkpoint_lock(dir: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir).map_err(|error| {
        format!(
            "create ANN bridge checkpoint directory {}: {error}",
            dir.display()
        )
    })?;
    let lock_path = dir.join(".bridge-checkpoint.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open ANN bridge lock {}: {error}", lock_path.display()))?;
    lock.lock()
        .map_err(|error| format!("acquire ANN bridge lock {}: {error}", lock_path.display()))?;
    Ok(lock)
}

async fn acquire_bridge_checkpoint_lock_async(
    dir: std::path::PathBuf,
) -> Result<std::fs::File, String> {
    tokio::task::spawn_blocking(move || acquire_bridge_checkpoint_lock(&dir))
        .await
        .map_err(|error| format!("ANN bridge lock task failed: {error}"))?
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ── persistence helpers ───────────────────────────────────────────────────────

/// Namespace key used in `retrieval_snapshots` for a given ns+model pair.
pub(crate) fn snapshot_key(namespace: &str, model: &str) -> String {
    format!("{namespace}::vamana::{model}")
}

/// Filesystem directory for v2 Vamana segment files for a given `(ns, model)` pair.
///
/// Returns `Some(<db-file>.ann/<hex>)` where `<hex>` is the lowercase hex encoding of
/// the bytes of `snapshot_key(ns, model)`, rooted beside the backing database file
/// (`backend_ann_root`) so co-located databases can never adopt each other's segments.
/// Hex encoding is injective, filesystem-safe, and reversible via
/// `decode_ann_dir_name`. Returns `None` for in-memory backends.
fn ann_segment_dir(rt: &KhiveRuntime, ns: &str, model: &str) -> Option<std::path::PathBuf> {
    let ann_root = rt.backend_ann_root()?;
    let key = snapshot_key(ns, model);
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    Some(ann_root.join(hex))
}

/// Decode a hex-encoded ann directory name back to `(namespace, model)`.
///
/// Reverses the encoding done by `ann_segment_dir`: hex-decodes `name` to bytes,
/// interprets them as UTF-8, then splits on `"::vamana::"`. Returns `None` on bad
/// hex, non-UTF-8 bytes, a missing separator, or empty namespace/model parts.
fn decode_ann_dir_name(name: &str) -> Option<(String, String)> {
    let raw = name.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    // `as_chunks` is unstable on stable; keep `chunks_exact` until it lands.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    for pair in raw.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
    }
    let key = String::from_utf8(bytes).ok()?;
    let (ns, model) = key.split_once("::vamana::")?;
    if ns.is_empty() || model.is_empty() {
        return None;
    }
    Some((ns.to_string(), model.to_string()))
}

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

const ANN_REBUILD_THRESHOLD_DEFAULT: f64 = 0.20;

/// `ann_rebuild_threshold` (ADR-079 Amendment 1 §5): the tail fraction of the
/// live vector count above which replay costs more than a full rebuild.
/// Values outside `(0, 1]` fall back to the default.
fn ann_rebuild_threshold() -> f64 {
    std::env::var("KHIVE_ANN_REBUILD_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0 && *v <= 1.0)
        .unwrap_or(ANN_REBUILD_THRESHOLD_DEFAULT)
}

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
            .execute(SqlStatement {
                sql: "INSERT OR IGNORE INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES (?1, ?2, ?3, ?4)"
                    .into(),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(ns.to_owned()),
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(ann_registry::PENDING_WATERMARK),
                ],
                label: Some("knowledge_ann_register_pathless_consumer".into()),
            })
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
            .execute(SqlStatement {
                sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES (?1, ?2, ?3, ?4) \
                      ON CONFLICT(consumer, namespace, embedding_model) \
                      DO UPDATE SET watermark = excluded.watermark"
                    .into(),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(key.namespace.clone()),
                    SqlValue::Text(key.model.clone()),
                    SqlValue::Integer(ann_registry::RECOVERING_WATERMARK),
                ],
                label: Some("knowledge_ann_mark_pathless_recovering".into()),
            })
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
    let mut reader = sql.reader().await.map_err(|e| e.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT watermark FROM ann_consumer_watermark \
                  WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                .into(),
            params: vec![
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(ns.to_owned()),
                SqlValue::Text(model.to_owned()),
            ],
            label: Some("ann_read_own_watermark".into()),
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
        let predicate = match shared_authority {
            WatermarkAuthority::PendingOrActive => {
                "(watermark = -2 OR (watermark >= 0 AND watermark <= ?4))"
            }
            WatermarkAuthority::Active => "watermark >= 0 AND watermark <= ?4",
            WatermarkAuthority::Recovering => "watermark = -1",
        };
        let mut writer = sql.writer().await.map_err(|error| error.to_string())?;
        writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE ann_consumer_watermark SET watermark = ?4 \
                     WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3 \
                       AND {predicate}"
                ),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(ns.to_owned()),
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(watermark),
                ],
                label: Some("knowledge_ann_raise_pathless_watermark".into()),
            })
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
            .execute(SqlStatement {
                sql: "DELETE FROM ann_write_log \
                      WHERE namespace = ?1 AND embedding_model = ?2 \
                        AND seq <= (SELECT MIN(watermark.watermark) \
                                    FROM ann_consumer_watermark watermark \
                                    WHERE (watermark.namespace = ?1 \
                                           OR watermark.namespace = '*') \
                                      AND watermark.embedding_model = ?2)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_owned()),
                    SqlValue::Text(model.to_owned()),
                ],
                label: Some("knowledge_ann_compact_pathless_log".into()),
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    ann_registry::compact_write_log(
        sql.as_ref(),
        CompactionScope::Namespace(ns.to_owned()),
        model,
    )
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
        .query_all(SqlStatement {
            sql: "SELECT EXISTS(SELECT 1 FROM ann_write_log \
                  WHERE namespace = ?1 AND embedding_model = ?2 \
                    AND field = 'knowledge.atom' AND seq > ?3) AS has_tail"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_owned()),
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
            ],
            label: Some("ann_tail_probe".into()),
        })
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
        .query_all(SqlStatement {
            sql: format!(
                "SELECT \
                   (SELECT COUNT(*) FROM {table_name} \
                     WHERE namespace = ?1 AND embedding_model = ?2 \
                       AND field = 'knowledge.atom') AS live, \
                   (SELECT COUNT(*) FROM ann_write_log \
                     WHERE namespace = ?1 AND embedding_model = ?2 \
                       AND field = 'knowledge.atom' AND seq > ?3) AS tail"
            ),
            params: vec![
                SqlValue::Text(ns.to_owned()),
                SqlValue::Text(model.to_owned()),
                SqlValue::Integer(s as i64),
            ],
            label: Some("ann_scope_counts".into()),
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
    let mut reverse = bridge.reverse_map();

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
            #[allow(clippy::chunks_exact_to_as_chunks)]
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

    let sql = rt.sql();
    let mut reader = sql.reader().await.map_err(|error| error.to_string())?;
    let rows = reader
        .query_all(SqlStatement {
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
                 ) \
                 SELECT selected.seq, selected.subject_id, selected.op, \
                        vectors.namespace AS vector_namespace, \
                        vectors.embedding_model AS vector_model, \
                        vectors.field AS vector_field, \
                        vectors.embedding, registry.registry_min, \
                        own.own_watermark, {live_column} AS live_count \
                 FROM registry CROSS JOIN own {live_join} \
                 LEFT JOIN selected ON 1 = 1 \
                 LEFT JOIN {table_name} AS vectors \
                   ON vectors.subject_id = selected.subject_id \
                 ORDER BY selected.seq"
            ),
            params,
            label: Some("knowledge_ann_fresh_tail_snapshot".into()),
        })
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
        #[allow(clippy::chunks_exact_to_as_chunks)]
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
                    candidates: merge_fresh_tail(Vec::new(), query, snapshot.ops),
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
        let bridge = match AnnBridge::load(&dir) {
            Ok(bridge) => bridge,
            Err(error) => {
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
        };
        let loaded_watermark = bridge
            .index
            .last_applied_seq()
            .unwrap_or(expected_watermark);
        let candidates = bridge.search(query, k);
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
                candidates: merge_fresh_tail(candidates, query, snapshot.ops),
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
                candidates: merge_fresh_tail(Vec::new(), query, snapshot.ops),
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
        .query_all(SqlStatement {
            sql: format!(
                "SELECT subject_id, embedding, \
                        (SELECT COALESCE(\
                           (SELECT seq FROM sqlite_sequence \
                            WHERE name = 'ann_write_log'), 0)) AS log_s \
                 FROM {table_name} \
                 WHERE namespace = ?1 AND embedding_model = ?2 \
                   AND field = 'knowledge.atom' \
                 ORDER BY subject_id"
            ),
            params: vec![SqlValue::Text(ns), SqlValue::Text(model_str)],
            label: Some("vamana_corpus_scan".into()),
        })
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
        #[allow(clippy::chunks_exact_to_as_chunks)]
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
/// Escape SQLite `LIKE` wildcard characters (`%`, `_`) and the escape
/// character itself (`\`) so a caller-supplied namespace is matched literally
/// under `LIKE ... ESCAPE '\'` rather than as a pattern (#819: an
/// underscore-bearing namespace like `a_b` must not also match `aXb`).
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) async fn invalidate_snapshot(rt: &KhiveRuntime, namespace: &str) {
    let pattern = format!("{}::vamana::%", escape_like(namespace));
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
    // itself is touched by exactly one statement in the whole decision path:
    // `scope_counts` below.
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

    // A tail exists — corpus-scale work is inherent from here. Rules 5, 7,
    // and 8 read (live, tail) from one snapshot.
    let (live, tail) = match scope_counts(rt, ns, model, s).await {
        Ok(counts) => counts,
        Err(e) => {
            tracing::warn!(error = %e, "ann scope-count read failed; Cold");
            return SegmentOutcome::Cold;
        }
    };

    // Rule 5: zero live corpus → Empty, regardless of tail contents.
    if live == 0 {
        tracing::info!(namespace = %ns, model = %model,
            "zero live corpus for scope; Empty (FTS/degraded path)");
        return SegmentOutcome::Empty;
    }

    // Rule 7: tail within threshold → Stale-tail: mmap load + final-state
    // replay, then checkpoint so the next restart's tail starts empty and the
    // served bridge returns to mmap backing.
    let threshold = (ann_rebuild_threshold() * live as f64).ceil() as u64;
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

    // Registration precedes every scan or legacy serve. Local absence of a
    // bridge cannot prove global first use: a peer may still hold stale v1 or
    // Owned state after this row was administratively removed. Every absent
    // knowledge row therefore publishes the durable force-rebuild sentinel
    // before any further serving, even in a fresh process.
    let mut force_rebuild = force_rebuild_required(ann, &key);
    match read_own_watermark(rt, &ns, model).await {
        Ok(Some(watermark)) if watermark < 0 => {
            mark_force_rebuild(ann, &key);
            force_rebuild = true;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
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
mod tests {
    use super::*;
    use khive_runtime::KhiveRuntime;
    use khive_storage::types::{SqlStatement, SqlValue};
    use serde_json::json;

    #[test]
    fn fresh_tail_merge_deduplicates_and_applies_deletes() {
        let deleted = Uuid::new_v4();
        let updated = Uuid::new_v4();
        let stable = Uuid::new_v4();
        let candidates = vec![(deleted, 0.99), (updated, 0.1), (stable, 0.5)];
        let ops = vec![(deleted, None), (updated, Some(vec![1.0, 0.0]))];

        let merged = merge_fresh_tail(candidates, &[1.0, 0.0], ops);

        assert!(
            !merged.iter().any(|(subject, _)| *subject == deleted),
            "a final tail delete must remove a stale ANN candidate"
        );
        assert_eq!(
            merged
                .iter()
                .filter(|(subject, _)| *subject == updated)
                .count(),
            1,
            "a tail upsert must replace, not duplicate, an ANN candidate"
        );
        assert_eq!(merged[0].0, updated, "the exact tail score must win");
        assert!(merged.iter().any(|(subject, _)| *subject == stable));
    }

    #[test]
    fn fresh_tail_merge_breaks_equal_scores_by_uuid() {
        let low = Uuid::from_u128(1);
        let middle = Uuid::from_u128(2);
        let high = Uuid::from_u128(3);
        let ops = vec![
            (high, Some(vec![1.0, 0.0])),
            (low, Some(vec![1.0, 0.0])),
            (middle, Some(vec![1.0, 0.0])),
        ];

        let merged = merge_fresh_tail(Vec::new(), &[1.0, 0.0], ops);

        assert_eq!(
            merged
                .into_iter()
                .map(|(subject, _)| subject)
                .collect::<Vec<_>>(),
            vec![low, middle, high],
            "equal-cosine fresh hits must not inherit HashMap iteration order"
        );
    }

    #[tokio::test]
    async fn fresh_tail_missing_registration_publishes_force_rebuild_sentinel() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let namespace = "local";
        let model = "fresh-tail-registration-test";
        let key = AnnKey::new(namespace, model);

        let outcome = force_cold_after_registry_loss(&rt, &ann, &key).await;

        assert!(matches!(
            outcome,
            FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
                if candidates.is_empty()
        ));
        assert_eq!(
            read_own_watermark(&rt, namespace, model)
                .await
                .expect("registry read"),
            Some(-1),
            "registry loss must publish the cross-process rebuild sentinel"
        );
        assert!(force_rebuild_required(&ann, &key));
    }

    #[tokio::test]
    async fn read_only_fresh_tail_registry_loss_stays_process_local() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = khive_runtime::RuntimeConfig {
            db_path: Some(dir.path().join("read-only-fresh-tail.db")),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        };
        drop(KhiveRuntime::new(config.clone()).expect("migrate snapshot source"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let db_path = config.db_path.as_ref().expect("db path");
            for suffix in ["-wal", "-shm"] {
                let mut name = db_path.file_name().expect("db file name").to_os_string();
                name.push(suffix);
                let sidecar = db_path.parent().expect("db parent dir").join(name);
                if sidecar.exists() {
                    let mut permissions = std::fs::metadata(&sidecar)
                        .expect("sidecar metadata")
                        .permissions();
                    permissions.set_mode(0o444);
                    std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
                }
            }
        }
        let rt = KhiveRuntime::new_readonly(config).expect("open snapshot read-only");
        let ann = new_shared();
        let namespace = "local";
        let model = "read-only-fresh-tail-model";
        let key = AnnKey::new(namespace, model);
        let before = rt.backend().pool().writer_acquisition_snapshot();

        let outcome = force_cold_after_registry_loss(&rt, &ann, &key).await;

        assert!(matches!(
            outcome,
            FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
                if candidates.is_empty()
        ));
        assert_eq!(
            read_own_watermark(&rt, namespace, model)
                .await
                .expect("registry read"),
            None,
            "read-only registry loss must not publish a rebuild sentinel"
        );
        assert!(
            !force_rebuild_required(&ann, &key),
            "read-only registry loss must not claim local rebuild authority"
        );
        assert_eq!(
            rt.backend().pool().writer_acquisition_snapshot(),
            before,
            "read-only fresh-tail degradation must not acquire a writer"
        );
    }

    #[tokio::test]
    async fn force_rebuild_queries_do_not_retire_authoritative_warm() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let key = AnnKey::new("local", "force-warm-test");
        force_cold_after_registry_loss(&rt, &ann, &key).await;
        simulate_warming_in_flight(&ann, key.clone());

        let outcome = fresh_tail_leg(&rt, &ann, &key, &[1.0], 1, None).await;

        assert!(matches!(
            outcome,
            FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
                if candidates.is_empty()
        ));
        assert!(
            is_warming_not_loaded(&ann, &key),
            "a concurrent degraded query must not invalidate the authoritative warm"
        );
    }

    /// #1150 regression: a tombstoned ordinal's stale id-map entry must not
    /// let a later replay op for the old (already-deleted) subject tombstone
    /// the slot a same-batch upsert just reused for a different subject.
    #[test]
    fn replay_does_not_tombstone_slot_reused_by_same_batch_upsert() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        let vectors = vec![
            1.0f32, 0.0, 0.0, // id_a, ordinal 0
            0.0, 1.0, 0.0, // id_b, ordinal 1
        ];
        let mut bridge = AnnBridge::build(vectors, 3, vec![id_a, id_b]).expect("build");

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
        let mut reverse = bridge.reverse_map();
        bridge
            .apply_final_op(&mut reverse, id_c, Some(vec![0.0f32, 0.0, 1.0]))
            .expect("apply upsert");
        bridge
            .apply_final_op(&mut reverse, id_a, None)
            .expect("apply delete");

        assert_eq!(
            bridge.id_map[0], id_c,
            "ordinal 0 must be owned by id_c after the replay"
        );
        assert!(
            !bridge.index.is_tombstoned(0),
            "id_a's stale delete must not tombstone the slot id_c now owns"
        );
        let hits = bridge.search(&[0.0, 0.0, 1.0], 2);
        assert!(
            hits.iter().any(|(id, score)| *id == id_c && *score > 0.9),
            "id_c must remain live and searchable, got: {hits:?}"
        );
        assert!(
            !hits.iter().any(|(id, _)| *id == id_a),
            "id_a must not resurface as a search hit, got: {hits:?}"
        );
    }

    #[tokio::test]
    async fn test_invalidate_snapshot_removes_vamana_rows() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        for (ns, idx_type) in &[
            ("local::vamana::model-a", "vamana"),
            ("local::vamana::model-b", "vamana"),
            ("local::hnsw::model-a", "hnsw"),
        ] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, 0)".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert");
        }
        drop(w);

        invalidate_snapshot(&rt, "local").await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"local::hnsw::model-a".to_string()),
            "HNSW rows must survive: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::vamana::model-a".to_string()),
            "vamana model-a must be deleted: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::vamana::model-b".to_string()),
            "vamana model-b must be deleted: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn test_invalidate_snapshot_does_not_cross_underscore_namespace() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        // "a_b" and "aXb" are distinct namespaces (the `_` in "a_b" is a
        // literal underscore, not a wildcard). Before #819's fix, invalidating
        // "a_b" also deleted "aXb"'s row because `_` is a single-character
        // LIKE wildcard.
        for ns in &["a_b::vamana::model-a", "aXb::vamana::model-a"] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, 0)".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text("vamana".to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert");
        }
        drop(w);

        invalidate_snapshot(&rt, "a_b").await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"aXb::vamana::model-a".to_string()),
            "unrelated namespace 'aXb' must survive invalidating 'a_b': {remaining:?}"
        );
        assert!(
            !remaining.contains(&"a_b::vamana::model-a".to_string()),
            "'a_b' own snapshot must still be deleted: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn test_invalidate_snapshot_tolerates_missing_table() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        // No retrieval_snapshots table — must not panic.
        invalidate_snapshot(&rt, "local").await;
    }

    #[tokio::test]
    async fn test_invalidate_clears_in_memory_ann() {
        let ann = new_shared();

        let dim = 4;
        let vectors = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let ids = vec![Uuid::new_v4(), Uuid::new_v4()];
        let bridge = AnnBridge::build(vectors, dim, ids).expect("build");
        let key = AnnKey::new("local", "test-model");
        assert!(
            insert_ann_if_absent(&ann, key.clone(), bridge).await,
            "insert must succeed on empty cache"
        );
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "pre-condition: ANN loaded"
        );

        clear_namespace(&ann, "local").await;
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "clearing SharedAnn must remove the bridge"
        );
    }

    #[tokio::test]
    async fn shared_ann_is_keyed_by_namespace_and_model() {
        let ann = new_shared();
        let model = "test-model";
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        let bridge_a = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![id_a])
            .expect("build namespace A bridge");
        let bridge_b = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![id_b])
            .expect("build namespace B bridge");

        assert!(insert_ann_if_absent(&ann, AnnKey::new("ns:a", model), bridge_a).await);
        assert!(insert_ann_if_absent(&ann, AnnKey::new("ns:b", model), bridge_b).await);

        let hits_b = search_loaded(&ann, &AnnKey::new("ns:b", model), &[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .expect("namespace B bridge exists");

        assert_eq!(hits_b.len(), 1);
        assert_eq!(
            hits_b[0].0, id_b,
            "namespace B query must not return namespace A neighbour"
        );
    }

    // ── generation-checked install (issue #770) ──────────────────────────────

    #[tokio::test]
    async fn install_if_fresher_rejects_late_stale_build() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        let fresh = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build fresh bridge")
            .with_generation(2);
        let stale = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge")
            .with_generation(1);

        // Fresh install first, then a late-arriving stale build must not clobber it.
        install_if_fresher(&ann, &key, fresh).await;
        install_if_fresher(&ann, &key, stale).await;

        let installed_generation = ann
            .indexes
            .read()
            .await
            .get(&key)
            .expect("entry present")
            .generation;
        assert_eq!(
            installed_generation, 2,
            "stale build (generation 1) must not replace fresher installed entry (generation 2)"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_accepts_forward_progress() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        let old = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build old bridge")
            .with_generation(1);
        let newer = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build newer bridge")
            .with_generation(2);

        // Normal forward progress: old installs first, newer build replaces it.
        install_if_fresher(&ann, &key, old).await;
        install_if_fresher(&ann, &key, newer).await;

        let installed_generation = ann
            .indexes
            .read()
            .await
            .get(&key)
            .expect("entry present")
            .generation;
        assert_eq!(
            installed_generation, 2,
            "newer build must replace an older installed entry"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_ties_keep_incumbent() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        let first = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build first bridge")
            .with_generation(1);
        let second_id = Uuid::new_v4();
        let second = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![second_id])
            .expect("build second bridge")
            .with_generation(1);

        install_if_fresher(&ann, &key, first).await;
        install_if_fresher(&ann, &key, second).await;

        let hits = search_loaded(&ann, &key, &[0.0, 1.0, 0.0, 0.0], 1)
            .await
            .expect("entry present");
        assert_ne!(
            hits.first().map(|(id, _)| *id),
            Some(second_id),
            "equal-generation candidate must not replace the incumbent"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_installs_into_empty_slot() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        let bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build bridge")
            .with_generation(0);

        install_if_fresher(&ann, &key, bridge).await;

        assert!(
            ann.indexes.read().await.contains_key(&key),
            "first successful build must always install into an empty slot"
        );
    }

    #[tokio::test]
    async fn clear_namespace_bumps_generation_scoped_to_namespace() {
        let ann = new_shared();

        assert_eq!(current_generation(&ann, "ns:a"), 0);
        assert_eq!(current_generation(&ann, "ns:b"), 0);

        clear_namespace(&ann, "ns:a").await;

        assert_eq!(
            current_generation(&ann, "ns:a"),
            1,
            "clear_namespace must bump the invalidated namespace's generation"
        );
        assert_eq!(
            current_generation(&ann, "ns:b"),
            0,
            "clear_namespace must not affect a different namespace's generation"
        );
    }

    #[tokio::test]
    async fn stale_build_installs_before_invalidation_race_is_rejected_after() {
        // Simulates the #770 race deterministically: build A (slow, e.g. the
        // full corpus rebuild fallthrough) starts scanning and captures its
        // generation floor. An invalidating write lands mid-build, clearing
        // the slot and bumping the namespace generation. The empty slot lets
        // a second, concurrent build B (e.g. `ensure_ann_background` retried
        // by the next search, since `clear_namespace` also freed the warming
        // guard) start, scan the now-current corpus, and install first. Only
        // afterward does build A's slow scan finish and attempt to install
        // its stale result — it must lose to B rather than clobbering it, the
        // exact bug this issue reports (`entry().or_insert()` would have let
        // A's late install win regardless of arrival order).
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        // Build A starts: capture the generation floor before doing any work.
        let build_a_generation = current_generation(&ann, "local");
        assert_eq!(build_a_generation, 0);

        // A concurrent write invalidates the namespace while A is still scanning.
        clear_namespace(&ann, "local").await;
        assert_eq!(current_generation(&ann, "local"), 1);

        // Build B starts after the invalidation (slot is empty, warming guard
        // was cleared too), scans the current corpus, and installs first.
        let build_b_generation = current_generation(&ann, "local");
        let build_b_id = Uuid::new_v4();
        let build_b_bridge = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![build_b_id])
            .expect("build fresh bridge")
            .with_generation(build_b_generation);
        install_if_fresher(&ann, &key, build_b_bridge).await;
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "build B (post-invalidation generation) must install"
        );

        // Build A's slow scan finally finishes and attempts to install its
        // stale (pre-invalidation) result.
        let build_a_bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge")
            .with_generation(build_a_generation);
        install_if_fresher(&ann, &key, build_a_bridge).await;

        let hits = search_loaded(&ann, &key, &[0.0, 1.0, 0.0, 0.0], 1)
            .await
            .expect("entry present");
        assert_eq!(
            hits.first().map(|(id, _)| *id),
            Some(build_b_id),
            "build A's late, stale install must not clobber build B's fresher result"
        );
    }

    #[tokio::test]
    async fn stale_build_rejected_installing_into_still_empty_post_invalidation_slot() {
        // Deterministic reproduction of the #770 scenario through the EMPTY-SLOT
        // door (PR #815): unlike the test above (where a fresh build
        // B installs first, so the stale build has an incumbent to lose against),
        // this exercises the case where NOTHING has installed yet when the stale
        // build arrives. Build A captures its generation floor, an invalidating
        // write (`clear_namespace`) bumps the namespace's generation while the
        // slot is still empty, and only then does A's late, stale install attempt
        // land — straight into that still-empty slot. The old `install_if_fresher`
        // compared a candidate only against an *existing* entry, so an empty slot
        // meant nothing to compare against and the stale build installed
        // unconditionally. The fix compares against the namespace's CURRENT
        // generation instead, so this must be rejected even with no incumbent.
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        // Build A starts: capture the generation floor before doing any work.
        let build_a_generation = current_generation(&ann, "local");
        assert_eq!(build_a_generation, 0);

        // An invalidating write lands while A is still scanning. The slot was
        // never populated, so this is a no-op on the map, but it must still
        // bump the namespace's generation.
        clear_namespace(&ann, "local").await;
        assert_eq!(current_generation(&ann, "local"), 1);
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "precondition: slot must still be empty after clear_namespace"
        );

        // Build A's slow scan finally finishes and attempts to install its
        // stale (pre-invalidation) result into the still-empty slot.
        let build_a_bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge")
            .with_generation(build_a_generation);
        install_if_fresher(&ann, &key, build_a_bridge).await;

        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "stale pre-invalidation build must not install into the emptied slot, \
             even with no incumbent to compare against"
        );
        assert!(
            search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
                .await
                .is_none(),
            "the fast path must not serve a stale index that was correctly rejected at install"
        );
    }

    // ── is_warming_not_loaded ─────────────────────────────────────────────────

    #[test]
    fn is_warming_false_when_neither_warming_nor_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "key absent from both sets must return false"
        );
    }

    #[test]
    fn is_warming_true_when_in_warming_but_not_indexes() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        simulate_warming_in_flight(&ann, key.clone());
        assert!(
            is_warming_not_loaded(&ann, &key),
            "key in warming but not indexes must return true"
        );
    }

    #[tokio::test]
    async fn is_warming_false_when_both_warming_and_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        // Mark as warming.
        simulate_warming_in_flight(&ann, key.clone());
        // Now insert the index (simulates background warm completing).
        let bridge =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
        insert_ann_if_absent(&ann, key.clone(), bridge).await;
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "key in both warming and indexes must return false (warm is done)"
        );
    }

    // ── wait_ready ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_ready_returns_true_immediately_when_already_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        let bridge =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
        insert_ann_if_absent(&ann, key.clone(), bridge).await;
        // Already loaded — should return true without sleeping.
        let ready = wait_ready(&ann, &key, 100, 10).await;
        assert!(ready, "must return true when index is already in the map");
    }

    #[tokio::test]
    async fn wait_ready_returns_false_on_timeout_when_never_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        // Nothing inserted — should time out and return false.
        let ready = wait_ready(&ann, &key, 60, 10).await;
        assert!(
            !ready,
            "must return false when index never appears within timeout"
        );
    }

    #[tokio::test]
    async fn wait_ready_returns_true_when_index_appears_mid_poll() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        let ann2 = ann.clone();
        let key2 = key.clone();
        // Spawn a task that inserts the bridge after a short delay.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
                .expect("build");
            insert_ann_if_absent(&ann2, key2, bridge).await;
        });
        // Poll with a 500ms timeout; the insert happens at ~40ms so it should succeed.
        let ready = wait_ready(&ann, &key, 500, 10).await;
        assert!(ready, "must return true when index appears before timeout");
    }

    // ── unavailable marker: terminal warm outcome (issue #1026) ──────────────

    #[tokio::test]
    async fn wait_ready_returns_false_immediately_when_marked_unavailable() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        mark_unavailable(&ann, &key, current_generation(&ann, "local"));

        let start = std::time::Instant::now();
        // Timeout is generous (5s, matching production ANN_WARM_WAIT_TIMEOUT_MS)
        // to prove the short-circuit fires rather than the deadline.
        let ready = wait_ready(&ann, &key, 5_000, 50).await;
        let elapsed = start.elapsed();

        assert!(
            !ready,
            "must return false for a key marked unavailable at the current generation"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "terminal unavailable outcome must short-circuit, not poll out the timeout: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn wait_ready_resumes_polling_when_unavailable_marker_is_stale() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        // Mark unavailable at generation 0, then bump the namespace's
        // generation past it so the marker is stale on the next check.
        mark_unavailable(&ann, &key, 0);
        clear_namespace(&ann, "local").await;
        assert_eq!(current_generation(&ann, "local"), 1);

        let ann2 = ann.clone();
        let key2 = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
                .expect("build")
                .with_generation(1);
            install_if_fresher(&ann2, &key2, bridge).await;
        });

        let ready = wait_ready(&ann, &key, 500, 10).await;
        assert!(
            ready,
            "a stale unavailable marker must not block polling; the index installed \
             mid-poll must still be observed"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_clears_unavailable_marker_on_successful_install() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        mark_unavailable(&ann, &key, 0);

        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build")
            .with_generation(0);
        install_if_fresher(&ann, &key, bridge).await;

        assert!(
            !unavailable_guard(&ann.unavailable).contains_key(&key),
            "a successful install must clear the unavailable marker for its key"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_stale_reject_does_not_clear_unavailable_marker() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        // Bump the namespace generation past the marker AND past the candidate,
        // so install_if_fresher rejects the candidate outright.
        clear_namespace(&ann, "local").await;
        mark_unavailable(&ann, &key, current_generation(&ann, "local"));

        let stale = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build")
            .with_generation(0);
        install_if_fresher(&ann, &key, stale).await;

        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "stale candidate must not install"
        );
        assert!(
            unavailable_guard(&ann.unavailable).contains_key(&key),
            "a rejected (non-installed) candidate must not clear the unavailable marker"
        );
    }

    // ── poison recovery ───────────────────────────────────────────────────────

    /// Poison the warm-state Mutex by panicking while holding the guard, then
    /// verify that `warm_states_guard` and callers built on it survive and
    /// return sane results.
    ///
    /// This test WOULD panic if `warm_states_guard` were reverted to
    /// `.expect("warm-state lock")`, because a poisoned Mutex causes `lock()`
    /// to return `Err`, and `.expect()` converts that to a panic.
    #[test]
    fn warm_states_guard_recovers_from_poison() {
        let ann = new_shared();
        let key = AnnKey::new("poison-ns", "poison-model");

        // Poison the mutex by sharing the Ann via Arc across a thread that panics
        // while holding the guard.
        let ann2 = ann.clone();
        let join_result = std::thread::spawn(move || {
            let _guard = ann2.warm_states.lock().expect("pre-poison lock");
            panic!("deliberate poison");
        })
        .join();
        assert!(join_result.is_err(), "poison thread must have panicked");
        assert!(
            ann.warm_states.is_poisoned(),
            "mutex must be poisoned before recovery"
        );

        // `warm_states_guard` must recover the guard without panicking.
        let guard = warm_states_guard(&ann.warm_states);
        assert!(
            !guard.contains_key(&key),
            "recovered guard must report key absent"
        );
        drop(guard);

        // Higher-level callers built on `warm_states_guard` must also succeed.
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "is_warming_not_loaded must not panic on poisoned Mutex"
        );
    }

    // ── shared warm-state machine (issue #566) ────────────────────────────────

    #[tokio::test]
    async fn warm_state_success_becomes_ready_and_suppresses_duplicates() {
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-unify-model");

        let permit = begin_warm(&ann, key.clone()).expect("Absent -> Warming");
        assert!(
            is_warming_not_loaded(&ann, &key),
            "owned Warming state without an index must report in flight"
        );
        assert!(
            begin_warm(&ann, key.clone()).is_none(),
            "a current Warming owner must singleflight duplicate callers"
        );

        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build bridge for warm-path test");
        insert_ann_if_absent(&ann, key.clone(), bridge).await;
        finish_warm(permit, AnnWarmOutcome::Ready).await;

        assert!(
            !is_warming_not_loaded(&ann, &key),
            "Ready state must not report warming in flight"
        );
        assert!(
            matches!(
                warm_states_guard(&ann.warm_states).get(&key),
                Some(AnnWarmState::Ready { generation: 0 })
            ),
            "a published index must finish in Ready"
        );
        assert!(
            begin_warm(&ann, key).is_none(),
            "Ready at the current generation must suppress redundant loads"
        );
    }

    #[tokio::test]
    async fn warm_state_failed_and_empty_outcomes_remain_retryable() {
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-unify-fail-model");

        let failed = begin_warm(&ann, key.clone()).expect("first warm");
        finish_warm(failed, AnnWarmOutcome::Failed).await;
        assert!(
            matches!(
                warm_states_guard(&ann.warm_states).get(&key),
                Some(AnnWarmState::Failed {
                    error: AnnWarmFailure::Operational,
                    ..
                })
            ),
            "no index and no empty marker is an operational failure"
        );

        let empty_retry = begin_warm(&ann, key.clone()).expect("Failed -> Warming retry");
        mark_unavailable(&ann, &key, current_generation(&ann, "local"));
        finish_warm(empty_retry, AnnWarmOutcome::Empty).await;
        assert!(
            matches!(
                warm_states_guard(&ann.warm_states).get(&key),
                Some(AnnWarmState::Failed {
                    error: AnnWarmFailure::EmptyCorpus,
                    ..
                })
            ),
            "current-generation empty scan must retain its distinct failure reason"
        );

        assert!(
            !is_warming_not_loaded(&ann, &key),
            "Failed must not masquerade as an in-flight warm"
        );
        let retry = begin_warm(&ann, key).expect("empty failures must remain retryable");
        drop(retry);
    }

    #[tokio::test]
    async fn failed_replacement_stays_retryable_with_servable_stale_index() {
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-stale-retry-model");
        let permit = begin_warm(&ann, key.clone()).expect("replacement warm");

        // ADR-079 rule 8 serves a stale bridge while its replacement rebuilds.
        let stale = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge");
        insert_ann_if_absent(&ann, key.clone(), stale).await;
        finish_warm(permit, AnnWarmOutcome::Failed).await;

        assert!(
            search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
                .await
                .is_some(),
            "the stale fallback must remain available to search"
        );
        assert!(
            begin_warm(&ann, key).is_some(),
            "a failed replacement must retry despite the servable stale fallback"
        );
    }

    #[tokio::test]
    async fn stale_finish_cannot_steal_new_post_invalidation_owner() {
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-owner-model");

        let stale = begin_warm(&ann, key.clone()).expect("warm A");
        clear_namespace(&ann, "local").await;
        let current = begin_warm(&ann, key.clone()).expect("warm B after invalidation");
        let current_id = current.attempt_id;

        finish_warm(stale, AnnWarmOutcome::Failed).await;
        assert!(
            matches!(
                warm_states_guard(&ann.warm_states).get(&key),
                Some(AnnWarmState::Warming { attempt_id, .. }) if *attempt_id == current_id
            ),
            "warm A's late cleanup must not erase or complete warm B's ownership"
        );
        assert!(
            begin_warm(&ann, key.clone()).is_none(),
            "warm B must remain the only current singleflight owner"
        );

        finish_warm(current, AnnWarmOutcome::Failed).await;
        assert!(
            begin_warm(&ann, key).is_some(),
            "warm B's failed completion must make the slot retryable"
        );
    }

    #[test]
    fn dropped_warm_permit_cannot_leave_stale_warming_ownership() {
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-cancel-model");

        let permit = begin_warm(&ann, key.clone()).expect("warm attempt");
        drop(permit);

        assert!(
            matches!(
                warm_states_guard(&ann.warm_states).get(&key),
                Some(AnnWarmState::Failed {
                    error: AnnWarmFailure::Interrupted,
                    ..
                })
            ),
            "cancellation must transition the owned slot out of Warming"
        );
        assert!(
            begin_warm(&ann, key).is_some(),
            "an interrupted warm must be retryable"
        );
    }

    // ── AnnBridge::save_atomic / load (slice 1b-i, ADR-079) ──────────────────

    fn build_test_bridge(dim: usize, n: usize) -> (AnnBridge, Vec<Uuid>) {
        let ids: Vec<Uuid> = (0..n).map(|_| Uuid::new_v4()).collect();
        let mut vectors: Vec<f32> = Vec::with_capacity(n * dim);
        for i in 0..n {
            for d in 0..dim {
                vectors.push(if d == i % dim { 1.0 } else { 0.0 });
            }
        }
        let bridge = AnnBridge::build(vectors, dim, ids.clone()).expect("build test bridge");
        (bridge, ids)
    }

    #[test]
    fn ann_bridge_save_atomic_load_round_trip() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let dim = 4;
        let (bridge, ids) = build_test_bridge(dim, 4);
        let first_id = ids[0];

        bridge.save_atomic(dir.path()).expect("save_atomic");

        let loaded = AnnBridge::load(dir.path()).expect("load");
        assert_eq!(
            loaded.num_vectors(),
            bridge.num_vectors(),
            "loaded vector count must match saved"
        );

        // Search with a query that points at vector 0 (1.0, 0.0, 0.0, 0.0)
        let query = vec![1.0f32, 0.0, 0.0, 0.0];
        let hits = loaded.search(&query, 1);
        assert_eq!(hits.len(), 1, "must return 1 hit");
        assert_eq!(hits[0].0, first_id, "top hit must be the first UUID");
    }

    #[test]
    fn ann_bridge_load_missing_sidecar_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (bridge, _) = build_test_bridge(4, 2);

        bridge.save_atomic(dir.path()).expect("save_atomic");
        std::fs::remove_file(dir.path().join("external_ids.bin")).expect("remove sidecar");

        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when external_ids.bin is missing"
        );
    }

    #[test]
    fn ann_bridge_load_torn_pair_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let dim = 4;

        // Save bridge A into the directory — both segments and sidecar for A.
        let (bridge_a, _) = build_test_bridge(dim, 2);
        bridge_a.save_atomic(dir.path()).expect("save_atomic A");

        // Overwrite the Vamana segments with bridge B's segments ONLY (no sidecar update).
        // This simulates a crash after VamanaIndex::save_atomic but before write_external_ids_sidecar.
        let (bridge_b, _) = build_test_bridge(dim, 3);
        bridge_b
            .index
            .save_atomic(dir.path())
            .expect("save_atomic B segments");

        // Now: metadata.bin is B's commit record, external_ids.bin is still bound
        // to A's commit-record digest.
        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when sidecar commit digest != on-disk commit record (torn pair)"
        );
        let err = result.err().expect("already asserted is_err");
        assert!(
            err.contains("commit-digest mismatch") || err.contains("torn"),
            "error message must mention digest mismatch or torn pair, got: {err}"
        );
    }

    #[test]
    fn ann_bridge_load_count_mismatch_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (bridge, _) = build_test_bridge(4, 2);
        bridge.save_atomic(dir.path()).expect("save_atomic");

        // Rewrite the sidecar via the codec itself: correctly bound to the
        // on-disk commit record, internally consistent, but carrying 99 UUIDs
        // instead of the index's 2 — only the count cross-check can catch it.
        let digest = segment_commit_digest(dir.path())
            .expect("digest ok")
            .expect("commit record present");
        let wrong_ids: Vec<uuid::Uuid> = (0..99).map(|_| uuid::Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &digest, &wrong_ids).expect("write patched sidecar");

        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when sidecar count != index.num_vectors()"
        );
    }

    #[test]
    fn ann_bridge_load_bad_magic_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (bridge, _) = build_test_bridge(4, 2);
        bridge.save_atomic(dir.path()).expect("save_atomic");

        // Overwrite the first 8 bytes with a wrong magic.
        let mut sidecar_bytes =
            std::fs::read(dir.path().join("external_ids.bin")).expect("read sidecar");
        sidecar_bytes[0..8].copy_from_slice(b"WRONGMAG");
        std::fs::write(dir.path().join("external_ids.bin"), &sidecar_bytes)
            .expect("write bad-magic sidecar");

        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when external_ids.bin has wrong magic"
        );
        let err = result.err().expect("already asserted is_err");
        assert!(
            err.contains("magic"),
            "error must mention magic mismatch, got: {err}"
        );
    }

    // ── slice 1b-ii-a: warm-path tests (ADR-079) ─────────────────────────────

    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use tempfile::TempDir;

    const WARM_TEST_MODEL: &str = "all-minilm-l6-v2";
    const WARM_DIMS: usize = 384;

    struct ConstVecService;

    #[async_trait]
    impl EmbeddingService for ConstVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; WARM_DIMS]).collect())
        }

        fn supports_model(&self, _: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "const-vec"
        }
    }

    struct TestEmbedderProvider;

    #[async_trait]
    impl EmbedderProvider for TestEmbedderProvider {
        fn name(&self) -> &str {
            WARM_TEST_MODEL
        }

        fn dimensions(&self) -> usize {
            WARM_DIMS
        }

        async fn build(&self) -> khive_runtime::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(ConstVecService))
        }
    }

    fn rt_with_embedder(db_path: Option<std::path::PathBuf>) -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("test runtime");
        rt.register_embedder(TestEmbedderProvider);
        rt
    }

    fn file_rt_with_embedder(db_path: std::path::PathBuf) -> KhiveRuntime {
        rt_with_embedder(Some(db_path))
    }

    fn memory_rt_with_embedder() -> KhiveRuntime {
        rt_with_embedder(None)
    }

    /// Seed `n` distinct rows into the vec0 table for `WARM_TEST_MODEL`.
    ///
    /// Calls `rt.vectors_for_model` first so the virtual table is created, then
    /// inserts raw f32 LE blobs directly via SQL.
    async fn seed_warm_corpus(rt: &KhiveRuntime, token: &NamespaceToken, n: usize) {
        seed_warm_corpus_opts(rt, token, n, true).await;
    }

    /// `log = false` seeds vec rows WITHOUT write-log rows — constructs the
    /// empty-log zero-watermark baseline state (a corpus that predates the
    /// migration's first logged write).
    async fn seed_warm_corpus_opts(rt: &KhiveRuntime, token: &NamespaceToken, n: usize, log: bool) {
        let _store = rt
            .vectors_for_model(token, WARM_TEST_MODEL)
            .expect("vec store");
        let model_key = sanitize_model_key(WARM_TEST_MODEL);
        let table = format!("vec_{model_key}");
        let ns = token.namespace().as_str().to_owned();
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        for i in 0..n {
            let id = Uuid::new_v4();
            let mut v = [0.0_f32; WARM_DIMS];
            v[i % WARM_DIMS] = 1.0;
            let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
            w.execute(SqlStatement {
                sql: format!(
                    "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text("concept".to_string()),
                    SqlValue::Text("knowledge.atom".to_string()),
                    SqlValue::Text(WARM_TEST_MODEL.to_string()),
                    SqlValue::Blob(bytes),
                ],
                label: None,
            })
            .await
            .expect("insert corpus row");
            if !log {
                continue;
            }
            // Mirror the production write path: every vector mutation appends
            // a write-log row in the same funnel (ADR-079 Amendment 1).
            w.execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(WARM_TEST_MODEL.to_string()),
                    SqlValue::Text("concept".to_string()),
                    SqlValue::Text("knowledge.atom".to_string()),
                    SqlValue::Text(id.to_string()),
                ],
                label: None,
            })
            .await
            .expect("append write-log row");
        }
    }

    async fn append_warm_vector(
        rt: &KhiveRuntime,
        token: &NamespaceToken,
        embedding: [f32; WARM_DIMS],
    ) -> Uuid {
        let _store = rt
            .vectors_for_model(token, WARM_TEST_MODEL)
            .expect("vec store");
        let table = format!("vec_{}", sanitize_model_key(WARM_TEST_MODEL));
        let namespace = token.namespace().as_str().to_owned();
        let subject = Uuid::new_v4();
        let bytes: Vec<u8> = embedding
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: format!(
                    "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, 'concept', 'knowledge.atom', ?3, ?4)"
                ),
                params: vec![
                    SqlValue::Text(subject.to_string()),
                    SqlValue::Text(namespace.clone()),
                    SqlValue::Text(WARM_TEST_MODEL.to_string()),
                    SqlValue::Blob(bytes),
                ],
                label: None,
            })
            .await
            .expect("insert fresh vector");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, 'concept', 'knowledge.atom', ?3, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Text(namespace),
                    SqlValue::Text(WARM_TEST_MODEL.to_string()),
                    SqlValue::Text(subject.to_string()),
                ],
                label: None,
            })
            .await
            .expect("append fresh-tail log row");
        subject
    }

    #[test]
    fn threshold_sized_tail_cap_scales_with_live_corpus() {
        let cap = |live: u64, threshold: f64| (live as f64 * threshold).ceil() as u64;
        assert_eq!(cap(3, 0.20), 1, "small corpora retain one newest row");
        assert_eq!(cap(5, 0.21), 2, "fractional limits round upward");
        assert_eq!(
            cap(1_000_000, 0.20),
            200_000,
            "large corpora must not collapse to the retired fixed 20k ceiling"
        );
    }

    #[tokio::test]
    async fn capped_snapshot_selects_newest_threshold_sized_suffix() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 5).await;
        register_consumer(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("register");

        let mut reader = rt.sql().reader().await.expect("reader");
        let newest_rows = reader
            .query_all(SqlStatement {
                sql: "SELECT subject_id FROM ann_write_log \
                      WHERE namespace = 'local' AND embedding_model = ?1 \
                        AND field = 'knowledge.atom' \
                      ORDER BY seq DESC LIMIT 2"
                    .into(),
                params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
                label: None,
            })
            .await
            .expect("latest log rows");
        let newest: Vec<Uuid> = newest_rows
            .iter()
            .map(|row| match row.get("subject_id") {
                Some(SqlValue::Text(subject)) => Uuid::parse_str(subject).expect("UUID"),
                other => panic!("unexpected subject: {other:?}"),
            })
            .collect();

        let one = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.20))
            .await
            .expect("20% snapshot");
        assert_eq!(one.live_count, Some(5));
        assert_eq!(one.ops.len(), 1, "ceil(5 × .20) = 1");
        assert_eq!(
            one.ops[0].0, newest[0],
            "the suffix must start newest-first"
        );

        let two = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.21))
            .await
            .expect("21% snapshot");
        assert_eq!(two.live_count, Some(5));
        assert_eq!(two.ops.len(), 2, "ceil(5 × .21) = 2");
        assert_eq!(
            two.ops
                .iter()
                .map(|(subject, _)| *subject)
                .collect::<Vec<_>>(),
            vec![newest[1], newest[0]],
            "selected newest rows are replayed in chronological order"
        );

        // A repeat op for the newest subject makes the two-row suffix contain
        // one distinct subject.  The final result must therefore coalesce to
        // one op, proving LIMIT is applied to raw newest writes before final-
        // state coalescing (the ADR-118 later-write boundary).
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES ('local', ?1, 'concept', 'knowledge.atom', ?2, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Text(WARM_TEST_MODEL.into()),
                    SqlValue::Text(newest[0].to_string()),
                ],
                label: None,
            })
            .await
            .expect("repeat newest write");
        drop(writer);
        let repeated = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.40))
            .await
            .expect("40% repeated snapshot");
        assert_eq!(
            repeated.ops.len(),
            1,
            "two newest writes coalesce to one subject"
        );
        assert_eq!(repeated.ops[0].0, newest[0]);
    }

    /// `ann_segment_dir` encodes a round-trippable hex key that `decode_ann_dir_name` reverses.
    #[tokio::test]
    async fn ann_segment_dir_encode_decode_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
            .expect("file backend must return Some(seg_dir)");

        let dir_name = seg_dir
            .file_name()
            .expect("seg_dir must have a basename")
            .to_string_lossy()
            .into_owned();

        let (decoded_ns, decoded_model) =
            decode_ann_dir_name(&dir_name).expect("decode must succeed for a valid encode");
        assert_eq!(decoded_ns, "local");
        assert_eq!(decoded_model, WARM_TEST_MODEL);

        // Parent directory is the database's own ANN root (`<db-file>.ann/`
        // beside the file), so co-located databases never share segments.
        let parent = seg_dir.parent().expect("seg_dir must have a parent");
        assert_eq!(
            parent.file_name().unwrap().to_string_lossy(),
            "test.db.ann",
            "seg_dir parent must be the database-scoped ANN root"
        );
    }

    /// `ensure_ann_for_model` must not panic on an in-memory runtime (no data_dir).
    #[tokio::test]
    async fn ensure_ann_no_data_dir_does_not_panic() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let token = rt.authorize(Namespace::local()).expect("authorize");
        // No data_dir → v2 path skipped. No corpus → no rebuild. Must complete silently.
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "no index should be loaded when corpus is empty and model is unknown"
        );
    }

    /// Cold-start build persists v2 segments; a second call restores from disk.
    ///
    /// Also gates the watermark contract: the persisted commit record must carry
    /// `last_applied_seq` covering the seeded writes, so the second call's
    /// classifier sees an empty tail and takes the Hot branch.
    #[tokio::test]
    async fn ensure_ann_round_trip_hot() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        // Cold-start: rebuild from corpus, persist v2 segments.
        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "first call must build the ANN index"
        );

        // Watermark contract: the extended commit record must carry a numeric
        // watermark covering every seeded write, so the tail above it is empty
        // and the Hot branch can fire.
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
            .expect("file backend must have a seg_dir");
        assert!(
            seg_dir.join("metadata.bin").exists(),
            "first call must persist v2 segments (metadata.bin missing)"
        );
        let info = read_commit_info(&seg_dir)
            .expect("read_commit_info must not err")
            .expect("metadata.bin must carry a v2 commit record");
        let s = info
            .last_applied_seq
            .expect("checkpoint must persist an extended record with a watermark");
        let (live, tail) = scope_counts(&rt, "local", WARM_TEST_MODEL, s)
            .await
            .expect("scope_counts must succeed");
        assert!(live > 0, "seeded corpus must be live");
        assert_eq!(
            tail, 0,
            "watermark must cover every seeded write (empty tail)"
        );

        // Hot path: load from persisted v2 segments without rebuilding. A rebuild
        // would call save_atomic and rewrite metadata.bin (new inode); a true Hot
        // load via AnnBridge::load never writes. Asserting the inode is unchanged
        // proves the second call took the v2 Hot branch, not a silent rebuild.
        use std::os::unix::fs::MetadataExt;
        let meta_path = seg_dir.join("metadata.bin");
        let ino_before = std::fs::metadata(&meta_path)
            .expect("metadata.bin must exist after first build")
            .ino();
        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        assert!(
            ann2.indexes.read().await.contains_key(&key),
            "second call must restore the ANN index from v2 segments"
        );
        let ino_after = std::fs::metadata(&meta_path)
            .expect("metadata.bin must still exist")
            .ino();
        assert_eq!(
            ino_before, ino_after,
            "second call must NOT rewrite metadata.bin — proves the v2 Hot load path, not a rebuild"
        );
    }

    #[tokio::test]
    async fn knowledge_search_and_suggest_merge_write_above_loaded_bridge_watermark() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let query = vec![1.0; WARM_DIMS];
        let (_, bridge_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .expect("serving bridge");

        let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
        let now = chrono::Utc::now().timestamp_micros();
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, tags, properties, status, \
                       finalized, created_at, updated_at) \
                      VALUES (?1, 'local', 'opaque-fresh-tail', 'Opaque Fresh Tail', \
                              'content with no lexical overlap', '[\"type:domain\"]', '{}', 'reviewed', \
                              1, ?2, ?2)"
                    .into(),
                params: vec![SqlValue::Text(fresh_id.to_string()), SqlValue::Integer(now)],
                label: None,
            })
            .await
            .expect("insert hydratable knowledge atom");
        drop(writer);

        let (_, still_loaded_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .expect("stale serving bridge remains loaded");
        assert_eq!(
            still_loaded_watermark, bridge_watermark,
            "the simulated external write must not mutate the in-process bridge"
        );

        let result = super::super::KnowledgeHandlers::search(
            &rt,
            &token,
            json!({
                "query": "quasar zephyr",
                "min_score": 0.1,
                "limit": 10,
                "rerank": false
            }),
            &ann,
        )
        .await
        .expect("knowledge.search");
        let ids: Vec<&str> = result["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|hit| hit["id"].as_str())
            .collect();
        let fresh_id = fresh_id.to_string();
        assert!(
            ids.contains(&fresh_id.as_str()),
            "fresh-tail atom must be visible without rebuilding the loaded bridge: {result}"
        );

        let suggest = super::super::KnowledgeHandlers::suggest(
            &rt,
            &token,
            json!({
                "query": "quasar zephyr nebula pulsar aurora",
                "limit": 10
            }),
            &ann,
        )
        .await
        .expect("knowledge.suggest");
        let suggest_ids: Vec<&str> = suggest["results"]
            .as_array()
            .expect("suggest results")
            .iter()
            .filter_map(|hit| hit["id"].as_str())
            .collect();
        assert!(
            suggest_ids.contains(&fresh_id.as_str()),
            "fresh-tail domain atom must be visible to knowledge.suggest: {suggest}"
        );
    }

    #[tokio::test]
    async fn fresh_tail_mismatch_replaces_stale_candidates_from_published_segment() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let query = vec![1.0; WARM_DIMS];
        let (old_candidates, old_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .expect("old serving bridge");

        let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
        assert!(
            !old_candidates
                .iter()
                .any(|(subject, _)| *subject == fresh_id),
            "the old bridge must not already contain the peer's fresh write"
        );

        // Simulate a peer checkpoint: publish a segment covering the fresh
        // write, raise the shared registry floor, and compact through it while
        // this process deliberately keeps its old bridge installed.
        let replacement = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("scan replacement corpus")
            .expect("replacement corpus is non-empty");
        let replacement_watermark = replacement
            .index
            .last_applied_seq()
            .expect("replacement carries a watermark");
        assert!(replacement_watermark > old_watermark);
        persist_ann_v2(&rt, "local", WARM_TEST_MODEL, &replacement)
            .expect("publish replacement segment");
        raise_watermark(
            &rt,
            "local",
            WARM_TEST_MODEL,
            replacement_watermark,
            CheckpointAuthority::Incremental,
        )
        .await
        .expect("raise peer watermark");
        compact_log(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("compact through peer watermark");
        assert!(
            !tail_exists(&rt, "local", WARM_TEST_MODEL, old_watermark)
                .await
                .expect("probe compacted old tail"),
            "the old bridge's tail must be gone so only segment re-resolution can recover it"
        );

        let generation_before = current_generation(&ann, "local");
        let outcome = fresh_tail_leg(&rt, &ann, &key, &query, 20, Some(old_watermark)).await;
        let replacement_candidates = match outcome {
            FreshTailOutcome::Replace { candidates, .. } => candidates,
            FreshTailOutcome::Ops(_) => {
                panic!("a compaction mismatch must replace, not extend, stale candidates")
            }
            FreshTailOutcome::Skipped => panic!("mismatch re-resolution must not disappear"),
        };

        assert!(
            replacement_candidates
                .iter()
                .any(|(subject, _)| *subject == fresh_id),
            "current-query re-resolution must surface the write covered by the published segment"
        );
        assert!(
            current_generation(&ann, "local") > generation_before,
            "mismatch handling must retire the stale cache generation"
        );
        assert!(
            search_loaded_with_seq(&ann, &key, &query, 20)
                .await
                .is_none(),
            "the stale in-process bridge must be evicted so normal warming re-adopts the segment"
        );
    }

    #[tokio::test]
    async fn registry_loss_evicts_stale_bridge_and_forces_authoritative_rebuild() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let query = vec![1.0; WARM_DIMS];
        let (_, stale_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .expect("serving bridge");
        let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;

        // Simulate administrative loss followed by a peer consumer compacting
        // the interval this process never checkpointed.  The stale bridge can
        // no longer be repaired from its `> S` tail.
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "DELETE FROM ann_consumer_watermark \
                      WHERE consumer = ?1 AND namespace = 'local' \
                        AND embedding_model = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(WARM_TEST_MODEL.into()),
                ],
                label: None,
            })
            .await
            .expect("delete own registry row");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES ('peer:test', 'local', ?1, 999)"
                    .into(),
                params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
                label: None,
            })
            .await
            .expect("insert peer watermark");
        drop(writer);
        compact_log(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("compact missing interval");
        assert!(
            !tail_exists(&rt, "local", WARM_TEST_MODEL, stale_watermark)
                .await
                .expect("tail probe"),
            "setup must remove the stale bridge's recovery tail"
        );

        let outcome = fresh_tail_leg(&rt, &ann, &key, &query, 20, Some(stale_watermark)).await;
        assert!(
            matches!(
                outcome,
                FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
                    if candidates.is_empty()
            ),
            "the query that detects registry loss must discard stale candidates"
        );
        assert!(search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .is_none());
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("registry read"),
            Some(-1),
            "the cross-process sentinel must stay durable through the Cold transition"
        );

        assert_eq!(
            ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
            AnnWarmOutcome::Ready
        );
        let rebuilt = search_loaded(&ann, &key, &query, 20)
            .await
            .expect("authoritative rebuilt bridge");
        assert!(
            rebuilt.iter().any(|(subject, _)| *subject == fresh_id),
            "the next warm must rebuild from the full corpus, not re-adopt the stale segment"
        );
        assert!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("registry read")
                .is_some_and(|watermark| watermark >= 0),
            "successful authoritative publication must clear the sentinel"
        );
        assert!(!force_rebuild_required(&ann, &key));
    }

    /// After a corpus mutation the persisted segment has a non-empty tail and the
    /// classifier replays it (Stale-tail), re-persisting a checkpoint that
    /// reflects the mutated corpus.
    #[tokio::test]
    async fn ensure_ann_stale_rebuild() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        // Initial build: persist v2 segments.
        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(ann.indexes.read().await.contains_key(&key), "initial build");

        // Mutate corpus: add one more row.
        seed_warm_corpus(&rt, &token, 1).await;

        // Gate: the mutation's logged write must sit above the persisted
        // watermark — the Stale-tail pre-condition.
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
            .expect("file backend must have a seg_dir");
        let info_before = read_commit_info(&seg_dir)
            .expect("read_commit_info must not err")
            .expect("v2 commit record must be present after initial build");
        let s_before = info_before
            .last_applied_seq
            .expect("initial checkpoint must carry a watermark");
        let (_, tail) = scope_counts(&rt, "local", WARM_TEST_MODEL, s_before)
            .await
            .expect("scope_counts must succeed");
        assert!(
            tail > 0,
            "mutation must appear as a tail row above the watermark"
        );

        // Fresh SharedAnn: non-empty tail detected → replay + checkpoint.
        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        assert!(
            ann2.indexes.read().await.contains_key(&key),
            "must serve an index after corpus mutation (tail replayed)"
        );

        // The post-replay checkpoint must reflect the mutated (5-row) corpus
        // and advance the watermark past the mutation's log row.
        let info_after = read_commit_info(&seg_dir)
            .expect("read_commit_info after replay must not err")
            .expect("v2 commit record must be present after replay checkpoint");
        assert_eq!(
            info_after.vector_count, 5,
            "checkpoint must reflect the 5-row corpus (4 initial + 1 replayed)"
        );
        let s_after = info_after
            .last_applied_seq
            .expect("replay checkpoint must carry a watermark");
        assert!(s_after > s_before, "checkpoint must advance the watermark");
        let (_, tail_after) = scope_counts(&rt, "local", WARM_TEST_MODEL, s_after)
            .await
            .expect("scope_counts must succeed");
        assert_eq!(
            tail_after, 0,
            "replayed tail must be covered by the new watermark"
        );
    }

    /// Review-mandated case: a checkpoint taken over an EMPTY log persists the
    /// zero watermark (the defined empty-log baseline), and the first logged
    /// write afterwards classifies Stale-tail — never Hot.
    #[tokio::test]
    async fn ensure_ann_zero_watermark_then_first_write_is_stale_tail() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        // Corpus WITHOUT log rows: the log is empty at checkpoint time.
        seed_warm_corpus_opts(&rt, &token, 4, false).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("seg_dir");
        let info = read_commit_info(&seg_dir)
            .expect("read_commit_info")
            .expect("v2 commit record");
        assert_eq!(
            info.last_applied_seq,
            Some(0),
            "empty-log checkpoint must persist the numeric zero baseline, not a missing watermark"
        );

        // First logged write after the zero-watermark checkpoint.
        seed_warm_corpus_opts(&rt, &token, 1, true).await;
        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let n = ann2
            .indexes
            .read()
            .await
            .get(&key)
            .map(AnnBridge::num_vectors)
            .expect("index must be served after the first logged write");
        assert_eq!(
            n, 5,
            "Stale-tail must replay the logged write (Hot would serve 4)"
        );
        let info2 = read_commit_info(&seg_dir)
            .expect("read_commit_info")
            .expect("v2 commit record after replay");
        assert!(
            info2.last_applied_seq.unwrap_or(0) > 0,
            "replay checkpoint must advance past the zero baseline"
        );
    }

    /// Review-mandated case: deleting every live vector classifies Empty — no
    /// ANN is served or replayed, and the terminal unavailable marker is set.
    #[tokio::test]
    async fn ensure_ann_delete_all_is_empty() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 3).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(ann.indexes.read().await.contains_key(&key), "initial build");

        // Delete every corpus row, logging each delete (production funnel shape).
        let table = format!("vec_{}", sanitize_model_key(WARM_TEST_MODEL));
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: format!(
                "INSERT INTO ann_write_log \
                 (namespace, embedding_model, kind, field, subject_id, op) \
                 SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                 FROM {table} WHERE namespace = ?1 AND embedding_model = ?2"
            ),
            params: vec![
                SqlValue::Text("local".into()),
                SqlValue::Text(WARM_TEST_MODEL.into()),
            ],
            label: None,
        })
        .await
        .expect("log deletes");
        w.execute(SqlStatement {
            sql: format!("DELETE FROM {table} WHERE namespace = ?1 AND embedding_model = ?2"),
            params: vec![
                SqlValue::Text("local".into()),
                SqlValue::Text(WARM_TEST_MODEL.into()),
            ],
            label: None,
        })
        .await
        .expect("delete corpus");
        drop(w);

        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        assert!(
            !ann2.indexes.read().await.contains_key(&key),
            "zero live corpus must classify Empty — no ANN served (rule 5 precedes Hot)"
        );
        assert!(
            is_terminally_unavailable(&ann2, &key),
            "Empty must set the terminal unavailable marker for wait_ready"
        );
    }

    /// Review-mandated interleaving: consumer A registers pending, checkpoints
    /// at S, and crashes before its raise — the pair MIN stays negative and another
    /// consumer's compaction cannot delete A's tail. After A's row is raised
    /// (or an overlapping row removed), compaction advances to the pair MIN.
    #[tokio::test]
    async fn compact_log_bounded_by_pair_minimum() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await; // seqs 1..=4 in the log

        register_consumer(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("register pending");
        // Overlapping consumer B durably checkpointed past every row.
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) VALUES (?1, ?2, ?3, 99)"
                    .into(),
                params: vec![
                    SqlValue::Text("other:test".into()),
                    SqlValue::Text("local".into()),
                    SqlValue::Text(WARM_TEST_MODEL.into()),
                ],
                label: None,
            })
            .await
            .expect("insert B row");
        }

        // A crashed before its raise: row is -2 → MIN(-2, 99) = -2 → nothing deletes.
        compact_log(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("compact");
        let (_, tail_while_pending) = scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
            .await
            .expect("scope_counts");
        assert_eq!(
            tail_while_pending, 4,
            "a fresh pending row must block pair compaction"
        );

        // A raises to 2 → MIN(2, 99) = 2 → rows 1-2 compact, 3-4 remain.
        raise_watermark(
            &rt,
            "local",
            WARM_TEST_MODEL,
            2,
            CheckpointAuthority::FullRegistered,
        )
        .await
        .expect("raise");
        compact_log(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("compact");
        let (_, tail_after) = scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
            .await
            .expect("scope_counts");
        assert_eq!(
            tail_after, 2,
            "compaction must advance exactly to the pair MIN"
        );
    }

    /// #1479 regression: a consumer which never published its first
    /// checkpoint blocks during its grace window, then retires visibly so an
    /// overlapping active consumer's watermark can bound compaction.
    #[tokio::test]
    async fn compact_log_retires_expired_never_activated_consumer() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        register_consumer(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("register pending");
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE ann_consumer_pending SET registered_at_us = 1 \
                      WHERE consumer = ?1 AND namespace = 'local' \
                        AND embedding_model = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(ANN_CONSUMER.into()),
                    SqlValue::Text(WARM_TEST_MODEL.into()),
                ],
                label: Some("test_age_pending_ann_consumer".into()),
            })
            .await
            .expect("age pending registration");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES ('other:test', 'local', ?1, 99)"
                    .into(),
                params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
                label: None,
            })
            .await
            .expect("insert active peer");
        drop(writer);

        compact_log(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("retire and compact");
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read retired registration"),
            None,
            "expired pending consumer must be removed so its return is Cold"
        );
        let (_, retained) = scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
            .await
            .expect("scope counts");
        assert_eq!(
            retained, 0,
            "the retired pending row must no longer pin the active peer's minimum"
        );
    }

    #[tokio::test]
    async fn ordinary_checkpoint_cannot_clear_force_rebuild_sentinel() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let ann = new_shared();
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        prepare_authoritative_rebuild(&rt, &ann, &key)
            .await
            .expect("publish sentinel");

        assert!(
            raise_watermark(
                &rt,
                "local",
                WARM_TEST_MODEL,
                7,
                CheckpointAuthority::Incremental,
            )
            .await
            .is_err(),
            "an ordinary checkpoint must lose the conditional publication fence"
        );
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read sentinel"),
            Some(-1)
        );

        raise_watermark(
            &rt,
            "local",
            WARM_TEST_MODEL,
            7,
            CheckpointAuthority::FullSentinel,
        )
        .await
        .expect("authoritative checkpoint clears sentinel");
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read raised watermark"),
            Some(7)
        );
    }

    #[tokio::test]
    async fn full_scan_checkpoint_clears_local_force_rebuild_marker() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        register_consumer(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("register pending consumer");
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read pending watermark"),
            Some(ann_registry::PENDING_WATERMARK)
        );
        let authority = prepare_full_corpus_scan(&rt, &ann, &key)
            .await
            .expect("establish full-scan authority");
        assert_eq!(authority, CheckpointAuthority::FullSentinel);
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read recovery fence"),
            Some(ann_registry::RECOVERING_WATERMARK),
            "a direct full scan must promote pending to the authoritative fence before scanning"
        );
        assert!(force_rebuild_required(&ann, &key));

        let bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("scan corpus")
            .expect("non-empty corpus");
        assert!(
            checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                bridge,
                current_generation(&ann, "local"),
                authority,
            )
            .await,
            "authoritative full scan must publish"
        );
        assert!(
            !force_rebuild_required(&ann, &key),
            "the shared checkpoint seam must finish local recovery for direct rebuild callers"
        );

        let query = vec![1.0; WARM_DIMS];
        let (_, watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .expect("published bridge");
        assert!(
            matches!(
                fresh_tail_leg(&rt, &ann, &key, &query, 20, Some(watermark)).await,
                FreshTailOutcome::Ops(_)
            ),
            "the next query must retain the freshly published bridge"
        );
    }

    #[tokio::test]
    async fn concurrent_full_sentinel_loser_does_not_restore_sentinel() {
        let rt = memory_rt_with_embedder();
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let authority_a = prepare_full_corpus_scan(&rt, &ann, &key)
            .await
            .expect("first full-scan authority");
        let authority_b = prepare_full_corpus_scan(&rt, &ann, &key)
            .await
            .expect("second full-scan authority");
        assert_eq!(authority_a, CheckpointAuthority::FullSentinel);
        assert_eq!(authority_b, CheckpointAuthority::FullSentinel);

        let bridge_a = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("scan A")
            .expect("non-empty A");
        let bridge_b = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("scan B")
            .expect("non-empty B");
        let generation = current_generation(&ann, "local");
        let (published_a, published_b) = tokio::join!(
            checkpoint_raise_compact_readopt(&rt, &ann, &key, bridge_a, generation, authority_a,),
            checkpoint_raise_compact_readopt(&rt, &ann, &key, bridge_b, generation, authority_b,)
        );

        assert!(published_a || published_b, "one full scan must win");
        assert!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read winner watermark")
                .is_some_and(|watermark| watermark >= 0),
            "the losing checkpoint must not demote the winner back to -1"
        );
        assert!(!force_rebuild_required(&ann, &key));
        assert!(
            has_current_index(&ann, &key).await,
            "the winner must remain available after the losing fence"
        );
    }

    #[tokio::test]
    async fn concurrent_pathless_normal_checkpoints_publish_monotonically() {
        let rt = memory_rt_with_embedder();
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;
        let ann = new_shared();
        assert_eq!(
            ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
            AnnWarmOutcome::Ready
        );

        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let old_authority = prepare_full_corpus_scan(&rt, &ann, &key)
            .await
            .expect("old authority");
        let old_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("old scan")
            .expect("old corpus");
        let old_watermark = old_bridge.index.last_applied_seq().unwrap_or(0);

        let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
        let new_authority = prepare_full_corpus_scan(&rt, &ann, &key)
            .await
            .expect("new authority");
        let new_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("new scan")
            .expect("new corpus");
        let new_watermark = new_bridge.index.last_applied_seq().unwrap_or(0);
        assert!(new_watermark > old_watermark);

        let generation = current_generation(&ann, "local");
        let (old_result, new_result) = tokio::join!(
            checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                old_bridge,
                generation,
                old_authority,
            ),
            checkpoint_raise_compact_readopt(
                &rt,
                &ann,
                &key,
                new_bridge,
                generation,
                new_authority,
            )
        );

        assert!(old_result && new_result);
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("registry read"),
            Some(i64::try_from(new_watermark).expect("watermark range"))
        );
        let query = vec![1.0; WARM_DIMS];
        let (hits, loaded_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .expect("monotone winner");
        assert_eq!(loaded_watermark, new_watermark);
        assert!(hits.iter().any(|(subject, _)| *subject == fresh_id));
    }

    /// In-memory SqlBridge writers share one connection. Lifecycle helpers
    /// must therefore remain single statements on pathless runtimes instead
    /// of trying to open a nested manual atomic-unit transaction.
    #[tokio::test]
    async fn pathless_lifecycle_helpers_do_not_open_nested_transactions() {
        let rt = memory_rt_with_embedder();
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let sql = rt.sql();
        let mut outer = sql.writer().await.expect("outer writer");
        outer
            .execute(SqlStatement {
                sql: "BEGIN IMMEDIATE".into(),
                params: vec![],
                label: Some("test_knowledge_pathless_outer_begin".into()),
            })
            .await
            .expect("begin outer transaction");

        register_consumer(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("register pending inside outer transaction");
        write_force_rebuild_sentinel_row(&rt, &key)
            .await
            .expect("publish recovery sentinel inside outer transaction");
        raise_watermark(
            &rt,
            "local",
            WARM_TEST_MODEL,
            0,
            CheckpointAuthority::FullSentinel,
        )
        .await
        .expect("activate sentinel inside outer transaction");
        compact_log(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("compact inside outer transaction");
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("read pathless lifecycle state"),
            Some(0)
        );

        outer
            .execute(SqlStatement {
                sql: "ROLLBACK".into(),
                params: vec![],
                label: Some("test_knowledge_pathless_outer_rollback".into()),
            })
            .await
            .expect("rollback outer transaction");
    }

    #[tokio::test]
    async fn stale_normal_checkpoint_adopts_newer_publisher_without_overwrite() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let initial_ann = new_shared();
        assert_eq!(
            ensure_ann_for_model(&rt, &token, &initial_ann, WARM_TEST_MODEL).await,
            AnnWarmOutcome::Ready
        );
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        let stale_authority = prepare_full_corpus_scan(&rt, &initial_ann, &key)
            .await
            .expect("stale authority");
        let stale_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("stale scan")
            .expect("stale corpus");
        let stale_watermark = stale_bridge.index.last_applied_seq().unwrap_or(0);

        let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
        let winner_ann = new_shared();
        let winner_authority = prepare_full_corpus_scan(&rt, &winner_ann, &key)
            .await
            .expect("winner authority");
        let winner_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("winner scan")
            .expect("winner corpus");
        let winner_watermark = winner_bridge.index.last_applied_seq().unwrap_or(0);
        assert!(winner_watermark > stale_watermark);
        assert!(
            checkpoint_raise_compact_readopt(
                &rt,
                &winner_ann,
                &key,
                winner_bridge,
                current_generation(&winner_ann, "local"),
                winner_authority,
            )
            .await,
            "newer publisher must commit"
        );

        let stale_ann = new_shared();
        assert!(
            checkpoint_raise_compact_readopt(
                &rt,
                &stale_ann,
                &key,
                stale_bridge,
                current_generation(&stale_ann, "local"),
                stale_authority,
            )
            .await,
            "stale publisher must adopt the winner instead of overwriting it"
        );
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("registry read"),
            Some(i64::try_from(winner_watermark).expect("watermark range"))
        );
        let info = read_commit_info(
            &ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("segment directory"),
        )
        .expect("commit read")
        .expect("commit info");
        assert_eq!(info.last_applied_seq, Some(winner_watermark));
        assert_eq!(info.vector_count, 5);
        let query = vec![1.0; WARM_DIMS];
        assert!(
            search_loaded(&stale_ann, &key, &query, 20)
                .await
                .expect("adopted winner")
                .iter()
                .any(|(subject, _)| *subject == fresh_id),
            "the stale publisher must serve the winner's fresh row"
        );
    }

    #[tokio::test]
    async fn first_use_empty_corpus_retains_cross_process_sentinel() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let ann = new_shared();
        let key = AnnKey::new("local", WARM_TEST_MODEL);

        assert_eq!(
            ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
            AnnWarmOutcome::Empty
        );
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("registry read"),
            Some(-1),
            "even a fresh process must preserve the only cross-process loss signal"
        );
        assert!(force_rebuild_required(&ann, &key));
    }

    #[tokio::test]
    async fn delayed_sentinel_request_cannot_demote_completed_recovery() {
        let rt = memory_rt_with_embedder();
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;
        let ann = new_shared();
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert_eq!(
            ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
            AnnWarmOutcome::Ready
        );
        let winner = read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("winner watermark")
            .expect("registered winner");
        assert!(winner >= 0);

        prepare_authoritative_rebuild(&rt, &ann, &key)
            .await
            .expect("delayed sentinel request");
        assert_eq!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL)
                .await
                .expect("registry read"),
            Some(winner),
            "revalidation under the publication lock must preserve the winner"
        );
        assert!(
            force_rebuild_required(&ann, &key),
            "the delayed detector must remain Cold until it scans or adopts authoritatively"
        );
    }

    /// Review-mandated case: a pre-amendment commit record (base length, no
    /// watermark trailer) classifies Cold and rebuilds — never serves Hot.
    #[tokio::test]
    async fn ensure_ann_pre_amendment_record_is_cold() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("seg_dir");

        // Truncate the 41-byte extended trailer: the record parses at the base
        // length — a legitimate pre-amendment commit with no watermark.
        let meta_path = seg_dir.join("metadata.bin");
        let bytes = std::fs::read(&meta_path).expect("read metadata.bin");
        std::fs::write(&meta_path, &bytes[..bytes.len() - 41]).expect("truncate trailer");
        let info = read_commit_info(&seg_dir)
            .expect("read_commit_info")
            .expect("base-length record must still parse");
        assert_eq!(
            info.last_applied_seq, None,
            "trailer removed → no watermark"
        );

        use std::os::unix::fs::MetadataExt;
        let ino_before = std::fs::metadata(&meta_path).expect("meta").ino();
        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann2.indexes.read().await.contains_key(&key),
            "Cold rebuild must still produce a served index"
        );
        let ino_after = std::fs::metadata(&meta_path).expect("meta").ino();
        assert_ne!(
            ino_before, ino_after,
            "pre-amendment record must force a rebuild (metadata.bin rewritten), not a Hot load"
        );
        let info2 = read_commit_info(&seg_dir)
            .expect("read_commit_info")
            .expect("rebuilt record");
        assert!(
            info2.last_applied_seq.is_some(),
            "rebuild must restore the extended watermark record"
        );
    }

    /// `ensure_ann_for_model`'s fast path must treat a present-but-generation-stale
    /// cached entry as a miss, not a hit (PR #815). In production
    /// `install_if_fresher`'s own fencing prevents a stale entry from ever
    /// installing, so this test bumps the namespace generation directly
    /// (bypassing `clear_namespace`'s eviction) to construct the "present but
    /// stale" state as an independent, defense-in-depth check on the fast path
    /// itself — mere presence must never again be trusted as freshness.
    #[tokio::test]
    async fn ensure_ann_fast_path_ignores_generation_stale_cached_entry() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "setup: first call must build and install the index at generation 0"
        );

        // Bump the namespace's generation directly, leaving the generation-0
        // entry present — the state install_if_fresher's fencing prevents in
        // production, exercised here purely to isolate the fast-path check.
        bump_generation(&ann, "local");
        assert_eq!(current_generation(&ann, "local"), 1);

        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;

        // If the in-memory fast path had (incorrectly) treated mere presence
        // as a hit, it would return immediately and the cached entry's
        // generation would still read 0. Falling through re-stamps it with
        // the namespace's current generation (1) via the v2/rebuild paths —
        // proof the stale entry was NOT served as a hit.
        assert_eq!(
            ann.indexes
                .read()
                .await
                .get(&key)
                .expect("entry present")
                .generation,
            1,
            "a present-but-generation-stale entry must not short-circuit via the fast \
             path; the reloaded/rebuilt entry must be re-stamped with the namespace's \
             new current generation"
        );
    }

    /// `warm_known_snapshots` must warm v2 segments even when the legacy
    /// `retrieval_snapshots` table is absent (the v1 query errors). Pre-fix it
    /// early-returned on that error and never reached the filesystem segment
    /// enumeration, so v2-only databases never warmed at daemon startup.
    #[tokio::test]
    async fn warm_known_snapshots_v2_only_no_legacy_table() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        // Setup: build + persist v2 segments to data_dir/ann/<hex>/.
        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "setup: first ensure must persist v2 segments"
        );

        // Force the worst case the fix targets: the v1 table is absent, so the
        // legacy query errors. Pre-fix, that error aborted the whole warm pass.
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: "DROP TABLE IF EXISTS retrieval_snapshots".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("drop retrieval_snapshots");
        }

        // Cold cache + warm: the v2 filesystem enumeration must still warm the
        // key despite the v1 query error.
        let ann_fresh = new_shared();
        warm_known_snapshots(&rt, &ann_fresh).await;
        assert!(
            ann_fresh.indexes.read().await.contains_key(&key),
            "warm_known_snapshots must warm v2 segments when retrieval_snapshots is absent \
             (regression: a v1 query error must not abort the v2 filesystem pass)"
        );
    }

    /// End-to-end reproduction of issue #1026: an empty corpus must leave the
    /// key marked unavailable so `wait_ready` short-circuits instead of
    /// polling out the full warm-wait timeout on every query.
    #[tokio::test]
    async fn ensure_ann_for_model_empty_corpus_marks_unavailable_and_wait_short_circuits() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        // No seed_warm_corpus call — the corpus stays empty for this model.

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "empty corpus must not install an index"
        );

        let start = std::time::Instant::now();
        let ready = wait_ready(&ann, &key, 5_000, 50).await;
        let elapsed = start.elapsed();

        assert!(!ready, "empty corpus must never become ready");
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "the terminal unavailable outcome must short-circuit the 5s warm-wait: {elapsed:?}"
        );
    }

    /// A rebuild error is operational, not proof of an unbuildable corpus:
    /// it must NOT leave an unavailable marker, so the retry the background
    /// path arranges (by removing the warming key) still gets a bounded wait
    /// instead of an instant `false` (issue #1026).
    #[tokio::test]
    async fn ensure_ann_for_model_rebuild_error_does_not_mark_unavailable() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 3).await;

        // Swap the corpus table for a view over a missing table so any scan
        // query fails operationally (SQLite validates views at query time).
        let model_key = sanitize_model_key(WARM_TEST_MODEL);
        let table = format!("vec_{model_key}");
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: format!("DROP TABLE {table}"),
            params: vec![],
            label: None,
        })
        .await
        .expect("drop corpus table");
        w.execute(SqlStatement {
            sql: format!("CREATE VIEW {table} AS SELECT * FROM missing_corpus_table"),
            params: vec![],
            label: None,
        })
        .await
        .expect("create broken view");
        drop(w);

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);

        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "a failed rebuild must not install an index"
        );
        assert!(
            !unavailable_guard(&ann.unavailable).contains_key(&key),
            "a rebuild ERROR must not mark the key unavailable — only a completed \
             empty-corpus scan may; a marker here would short-circuit wait_ready \
             while the same-generation retry is in flight"
        );

        // The next request's wait must still observe an index installed
        // mid-poll by the same-generation retry.
        let ann2 = ann.clone();
        let key2 = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
                .expect("build")
                .with_generation(0);
            install_if_fresher(&ann2, &key2, bridge).await;
        });
        let ready = wait_ready(&ann, &key, 500, 10).await;
        assert!(
            ready,
            "after a rebuild error the wait must keep polling and observe the \
             retry's install, not short-circuit false"
        );
    }

    /// A store-opening failure (here: a model with no registered embedder)
    /// must propagate as an error, not collapse into `Ok(None)` — otherwise
    /// it would be indistinguishable from a verified empty corpus and leave
    /// a terminal unavailable marker that blocks the same-generation retry.
    #[tokio::test]
    async fn ensure_ann_for_model_store_open_failure_does_not_mark_unavailable() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");

        let model = "model-with-no-registered-embedder";
        assert!(
            rt.vectors_for_model(&token, model).is_err(),
            "precondition: opening the vector store for an unregistered model must fail"
        );

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, model).await;
        let key = AnnKey::new("local", model);

        assert!(
            !unavailable_guard(&ann.unavailable).contains_key(&key),
            "a store-opening failure must not mark the key unavailable"
        );

        // The next request's wait must still observe a same-generation install.
        let ann2 = ann.clone();
        let key2 = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
                .expect("build")
                .with_generation(0);
            install_if_fresher(&ann2, &key2, bridge).await;
        });
        let ready = wait_ready(&ann, &key, 500, 10).await;
        assert!(
            ready,
            "after a store-opening failure the wait must keep polling and observe \
             the retry's install, not short-circuit false"
        );
    }
}
