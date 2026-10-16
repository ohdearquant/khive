//! Brain state persistence — snapshot upsert and namespace-scoped reload.

mod feedback;
pub(crate) use feedback::persist_feedback_state_mutation;

use crate::sql::{sql, sql_err};

use std::collections::HashMap;
use std::sync::Mutex;

// Test-only interleaving hook for ensure_loaded.
//
// When set, every cold ensure_loaded call (after the async DB load, before the
// final tracker lock) will:
//   1. Send () on `reached_tx` to signal the test controller "I am at the hook".
//   2. Await `proceed_rx` before continuing.
//
// This lets a test precisely inject the following interleaving:
//   - Loader B reaches hook  →  test controller receives reached signal
//   - Test controller runs Loader A to completion and mutates state
//   - Test controller sends on proceed_tx  →  Loader B continues
//
// With the old code (no re-check), Loader B then clobbered A's mutation.
// With the fix (re-check under final guard), Loader B sees is_active=true
// and returns early, preserving A's mutation.
#[cfg(test)]
pub(crate) struct LoadHook {
    pub reached_tx: tokio::sync::oneshot::Sender<()>,
    pub proceed_rx: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) static POST_LOAD_HOOK: std::sync::Mutex<Option<LoadHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_post_load_hook(hook: LoadHook) {
    *POST_LOAD_HOOK.lock().unwrap() = Some(hook);
}

#[cfg(test)]
pub(crate) fn clear_post_load_hook() {
    *POST_LOAD_HOOK.lock().unwrap() = None;
}

use serde_json::Value;

use khive_runtime::{EventAttribution, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::event::Event;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess, SqlReader, SqlWriter};
use khive_types::EventKind;

use khive_brain_core::{
    validate_brain_state_snapshot_with_capacity, BrainSignal, BrainState, BrainStateSnapshot,
};

/// Hook signals have no durable replay guarantee. A cold namespace retains
/// only its most recent useful signals until its first brain verb loads it.
pub(crate) const MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE: usize = 256;

use crate::event::interpret;

const SNAPSHOT_PROFILE_ID: &str = "__brain__";

fn version_covers(current: Option<i64>, observed: Option<i64>) -> bool {
    match (current, observed) {
        (Some(current), Some(observed)) => current >= observed,
        (Some(_), None) | (None, None) => true,
        (None, Some(_)) => false,
    }
}

/// Tracks loaded namespaces and owns the per-namespace state map.
pub struct PersistenceTracker {
    /// Namespace currently reflected in the shared `BrainState` slot, if any.
    pub(crate) active_namespace: Option<String>,
    /// Saved snapshots of in-memory state for namespaces that have been initialised
    /// but are not currently in the active slot.  Used for save-restore on switch.
    saved_states: HashMap<String, BrainState>,
    /// Namespaces for which state has been initialised (from DB or fresh default).
    pub(crate) loaded_namespaces: HashMap<String, ()>,
    /// Latest durable snapshot version observed for each loaded namespace.
    ///
    /// Snapshot `updated_at` is raised monotonically inside every full-state
    /// mutation transaction. A warm process compares this version before
    /// reusing its in-memory registry.
    snapshot_versions: HashMap<String, i64>,
    /// Pre-load accumulator for hook signals that arrive before `ensure_loaded` runs.
    ///
    /// When `on_dispatch` fires for a namespace that has never been loaded from the
    /// DB, the signal is queued here rather than applied to a speculative
    /// `BrainState`.  `ensure_loaded` drains this queue *after* the snapshot +
    /// event-replay path completes, so the ordering guarantee is:
    ///   persisted snapshot → replayed events → queued hook signals
    ///
    /// The namespace is deliberately NOT added to `loaded_namespaces` while
    /// signals are pending; that prevents `ensure_loaded` from skipping the DB
    /// round-trip for a namespace that may have existing persisted history.
    pending_hook_signals: HashMap<String, Vec<BrainSignal>>,
    /// Process-local count of useful cold-namespace signals discarded when a
    /// namespace's pending queue reached its ceiling.
    pending_hook_signals_dropped: u64,
}

impl Default for PersistenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistenceTracker {
    /// Create a fresh `PersistenceTracker` with no loaded namespaces.
    pub fn new() -> Self {
        Self {
            active_namespace: None,
            saved_states: HashMap::new(),
            loaded_namespaces: HashMap::new(),
            snapshot_versions: HashMap::new(),
            pending_hook_signals: HashMap::new(),
            pending_hook_signals_dropped: 0,
        }
    }

    /// Return `true` if `namespace` has been initialised (from DB or fresh default).
    pub fn is_loaded(&self, namespace: &str) -> bool {
        self.loaded_namespaces.contains_key(namespace)
    }

    /// Return `true` if `namespace` is the currently active namespace slot.
    pub fn is_active(&self, namespace: &str) -> bool {
        self.active_namespace.as_deref() == Some(namespace)
    }

    /// Register `namespace` as loaded and set it as the active namespace.
    pub fn mark_loaded(&mut self, namespace: String) {
        self.loaded_namespaces.insert(namespace.clone(), ());
        self.active_namespace = Some(namespace);
    }

    fn mark_loaded_at(&mut self, namespace: String, snapshot_version: Option<i64>) {
        match snapshot_version {
            Some(version) => {
                self.snapshot_versions.insert(namespace.clone(), version);
            }
            None => {
                self.snapshot_versions.remove(&namespace);
            }
        }
        self.mark_loaded(namespace);
    }

    fn snapshot_version(&self, namespace: &str) -> Option<i64> {
        self.snapshot_versions.get(namespace).copied()
    }

    /// Save `from_state` for `from_namespace`, then activate `to_namespace` (returning saved state if any).
    pub fn swap_namespace(
        &mut self,
        from_namespace: &str,
        from_state: BrainState,
        to_namespace: String,
    ) -> Option<BrainState> {
        self.saved_states
            .insert(from_namespace.to_string(), from_state);
        let saved = self.saved_states.remove(&to_namespace);
        self.active_namespace = Some(to_namespace);
        saved
    }

    /// Return the total_events counter for any namespace stored in `saved_states`
    /// (saved-off namespaces) or `pending_hook_signals` (cold pre-load queue).
    /// Returns `None` when no state has been initialised for the given namespace.
    ///
    /// Note: does NOT return the counter for the active namespace, which lives in
    /// the shared `BrainState` slot, not in `saved_states`.
    #[cfg(test)]
    pub(crate) fn total_events_for(&self, namespace: &str) -> Option<u64> {
        if let Some(s) = self.saved_states.get(namespace) {
            return Some(s.balanced_recall.total_events);
        }
        self.pending_hook_signals
            .get(namespace)
            .map(|signals| signals.len() as u64)
    }

    pub(crate) fn pending_hook_signals_dropped(&self) -> u64 {
        self.pending_hook_signals_dropped
    }

    /// Apply a signal to the state bucket that owns `namespace`.
    ///
    /// - Active namespace: returns `ApplyTarget::ActiveSlot` — caller must apply
    ///   the signal to the shared `BrainState` lock while holding the dispatch gate.
    /// - Saved (loaded) namespace: applies directly to `saved_states` and returns
    ///   `ApplyTarget::Done`.
    /// - Cold/unknown namespace: enqueues a useful signal in the bounded
    ///   `pending_hook_signals` queue and returns `ApplyTarget::Done`. The
    ///   namespace is NOT marked loaded; the DB round-trip is preserved for
    ///   the first `ensure_loaded` call. The queue is drained *after* snapshot
    ///   restore and event replay, so retained signals land afterward.
    ///
    /// `Irrelevant` is discarded. On cold-queue overflow, the oldest useful
    /// signal is dropped and counted; attribution policy may independently
    /// reject an unattributed or unavailable-profile signal after routing.
    pub(crate) fn route_signal(
        &mut self,
        namespace: &str,
        signal: &khive_brain_core::BrainSignal,
        entity_capacity: usize,
    ) -> ApplyTarget {
        if matches!(signal, BrainSignal::Irrelevant) {
            return ApplyTarget::Done;
        }
        if self.is_active(namespace) {
            return ApplyTarget::ActiveSlot;
        }

        if self.is_loaded(namespace) {
            // Namespace has been loaded from the DB but is currently saved off.
            // Apply directly to its saved BrainState.
            if let Some(state) = self.saved_states.get_mut(namespace) {
                crate::apply_dispatch_signal(state, signal);
            }
            return ApplyTarget::Done;
        }

        // Cold/unknown namespace: queue the signal for deferred application.
        // Do NOT mark the namespace loaded — ensure_loaded must still perform
        // the DB snapshot + event-replay before draining this queue.
        let _ = entity_capacity;
        let pending = self
            .pending_hook_signals
            .entry(namespace.to_string())
            .or_default();
        if pending.len() == MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE {
            pending.remove(0);
            self.pending_hook_signals_dropped = self.pending_hook_signals_dropped.saturating_add(1);
        }
        pending.push(signal.clone());
        ApplyTarget::Done
    }

    /// Drain and return any pending hook signals for `namespace`.
    ///
    /// Called by `ensure_loaded` after the normal load path (snapshot + replay)
    /// completes, so queued signals land on top of persisted history.
    pub(crate) fn drain_pending_signals(&mut self, namespace: &str) -> Vec<BrainSignal> {
        self.pending_hook_signals
            .remove(namespace)
            .unwrap_or_default()
    }
}

/// Indicates where a dispatched signal should be applied by the caller.
pub(crate) enum ApplyTarget {
    /// The namespace is active — the caller must apply the signal to the shared
    /// `BrainState` slot (which the caller already holds behind the dispatch gate).
    ActiveSlot,
    /// Signal was applied inside `PersistenceTracker`; caller has nothing left to do.
    Done,
}

#[cfg(test)]
#[path = "persist/pending_hook_signal_tests.rs"]
mod pending_hook_signal_tests;

#[cfg(test)]
#[path = "persist/sql_err_tests.rs"]
mod sql_err_tests;

/// Append one `brain_event_log` row using an already-acquired writer (plain
/// writer or an open transaction — both implement `SqlWriter`).
async fn append_brain_event_on_writer(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    profile_id: &str,
    event_kind: &str,
    payload: &Value,
    created_at_us: i64,
) -> Result<(), RuntimeError> {
    let payload_str = serde_json::to_string(payload)
        .map_err(|e| sql_err("brain persistence serialize event", e))?;

    writer
        .execute(SqlStatement {
            sql: sql!("brain_event_log_insert").into(),
            params: vec![
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(event_kind.to_string()),
                SqlValue::Text(payload_str),
                SqlValue::Integer(created_at_us),
            ],
            label: Some("brain_event_log_append".into()),
        })
        .await?;

    Ok(())
}

/// Upsert the namespace's `brain_profile_snapshots` row using an
/// already-acquired writer (plain writer or an open transaction).
async fn upsert_snapshot_on_writer(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    snapshot: &BrainStateSnapshot,
    updated_at_us: i64,
) -> Result<(), RuntimeError> {
    let snapshot_json = serde_json::to_string(snapshot)
        .map_err(|e| sql_err("brain persistence serialize snapshot", e))?;

    upsert_snapshot_json_on_writer(writer, namespace, snapshot_json, updated_at_us).await
}

async fn upsert_snapshot_json_on_writer(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    snapshot_json: String,
    updated_at_us: i64,
) -> Result<(), RuntimeError> {
    writer
        .execute(SqlStatement {
            sql: sql!("brain_profile_snapshot_upsert").into(),
            params: vec![
                SqlValue::Text(SNAPSHOT_PROFILE_ID.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(snapshot_json),
                SqlValue::Integer(updated_at_us),
            ],
            label: Some("brain_snapshot_upsert".into()),
        })
        .await?;

    Ok(())
}

pub async fn append_brain_event(
    sql: &dyn SqlAccess,
    namespace: &str,
    profile_id: &str,
    event_kind: &str,
    payload: &Value,
    created_at_us: i64,
) -> Result<(), RuntimeError> {
    let mut writer = sql.writer().await?;
    append_brain_event_on_writer(
        writer.as_mut(),
        namespace,
        profile_id,
        event_kind,
        payload,
        created_at_us,
    )
    .await
}

pub async fn upsert_snapshot(
    sql: &dyn SqlAccess,
    namespace: &str,
    snapshot: &BrainStateSnapshot,
    updated_at_us: i64,
) -> Result<(), RuntimeError> {
    let mut writer = sql.writer().await?;
    upsert_snapshot_on_writer(writer.as_mut(), namespace, snapshot, updated_at_us).await
}

/// Descriptor for a single durable `BrainState` mutation (issues #457/#458):
/// which brain-event-log row to append alongside the snapshot upsert.
pub struct BrainMutationEvent {
    pub profile_id: String,
    pub event_kind: String,
    pub payload: Value,
}

/// Public feedback write to join to the authoritative profile-state transaction.
pub(crate) enum FeedbackEventWrite {
    Direct(Event),
    Graded {
        event: Event,
        scorer_run_id: String,
        serve_ledger_id: String,
        grade: String,
    },
    Gated {
        event: Event,
        target_id: String,
        gate_mode: crate::fold_gate::FeedbackGateMode,
        gate_now_us: i64,
        dedup_key: Option<(String, String)>,
        grade: Option<String>,
    },
}

enum MutationOutcome<R> {
    Committed {
        result: R,
        state: Box<BrainState>,
        snapshot_version: i64,
    },
    Rejected(RuntimeError),
}

enum FeedbackMutationOutcome {
    Committed {
        event: Event,
        state: Box<BrainState>,
        snapshot_version: i64,
    },
    Deduped,
    Rejected(RuntimeError),
}

/// Select the transaction's authoritative snapshot without losing version-matched hook state.
async fn rebase_snapshot_on_writer<R: SqlReader + ?Sized>(
    writer: &mut R,
    namespace: &str,
    local_version: Option<i64>,
    local_snapshot: BrainStateSnapshot,
    entity_capacity: usize,
    context: (&'static str, &'static str),
    map_error: fn(&'static str, RuntimeError) -> khive_storage::StorageError,
) -> khive_storage::StorageResult<(BrainState, Option<i64>)> {
    let previous_updated_at = load_snapshot_version_on_reader(writer, namespace)
        .await
        .map_err(|e| map_error(context.0, e))?;
    let base_snapshot = match previous_updated_at {
        Some(updated_at) if local_version == Some(updated_at) => local_snapshot,
        Some(updated_at) => {
            let (durable_snapshot, reloaded_updated_at) =
                load_latest_snapshot_on_reader(writer, namespace, entity_capacity)
                    .await
                    .map_err(|e| map_error(context.1, e))?
                    .ok_or_else(|| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Sql,
                            context.1,
                            std::io::Error::other(
                                "snapshot disappeared inside the write transaction",
                            ),
                        )
                    })?;
            if reloaded_updated_at != updated_at {
                return Err(khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    context.1,
                    std::io::Error::other(format!(
                        "snapshot generation changed inside the write transaction: \
                         expected {updated_at}, got {reloaded_updated_at}"
                    )),
                ));
            }
            durable_snapshot
        }
        None => local_snapshot,
    };
    Ok((
        BrainState::from_snapshot(base_snapshot, entity_capacity),
        previous_updated_at,
    ))
}

/// Apply a `BrainState` mutation with durability as part of the success
/// contract (issues #457/#458).
///
/// `mutate` runs against a *proposed* state whose generation is verified after
/// the write transaction begins (never against the live `state` mutex). A
/// version-matched local copy preserves best-effort hook updates; a mismatch
/// rebases on the latest durable snapshot. Its result and the resulting
/// snapshot are then persisted — brain event-log append AND
/// snapshot upsert — as ONE atomic unit via `SqlAccess::atomic_unit`. Only
/// after that unit commits does the proposed state replace the live state
/// and the tracker record the committed version. If `mutate` fails, or the atomic unit
/// fails to append, upsert, or commit, this returns `Err` and the live state
/// is left completely untouched — there is no in-memory mutation to roll
/// back because it was never applied to the shared state in the first
/// place. See `crates/khive-pack-brain/docs/api/persist.md` for why this
/// takes `&dyn SqlAccess` and why it uses `atomic_unit` over a manual
/// transaction.
pub async fn persist_brain_state_mutation<R: Send + 'static>(
    sql: &dyn SqlAccess,
    token: &NamespaceToken,
    tracker: &Mutex<PersistenceTracker>,
    state: &Mutex<BrainState>,
    event: BrainMutationEvent,
    entity_capacity: usize,
    mutate: impl FnOnce(&mut BrainState) -> Result<R, RuntimeError> + Send + 'static,
) -> Result<R, RuntimeError> {
    let namespace = token.namespace().as_str().to_string();
    let now_us = chrono::Utc::now().timestamp_micros();
    let (local_version, local_snapshot) = {
        let tracker = tracker.lock().unwrap();
        let version = tracker.snapshot_version(&namespace);
        let snapshot = state.lock().unwrap().to_snapshot();
        (version, snapshot)
    };

    let namespace_for_op = namespace.clone();
    let profile_id = event.profile_id;
    let event_kind = event.event_kind;
    let payload = event.payload;
    let op: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let (mut proposed, previous_updated_at) = rebase_snapshot_on_writer(
                writer,
                &namespace_for_op,
                local_version,
                local_snapshot,
                entity_capacity,
                (
                    "brain_persist_load_snapshot_version",
                    "brain_persist_load_snapshot",
                ),
                |step, error| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        step,
                        error,
                    )
                },
            )
            .await?;

            let result = match mutate(&mut proposed) {
                Ok(result) => result,
                Err(error) => {
                    return Ok(Box::new(MutationOutcome::<R>::Rejected(error))
                        as Box<dyn std::any::Any + Send>);
                }
            };
            let commit_at_us = previous_updated_at
                .map(|updated_at| now_us.max(updated_at.saturating_add(1)))
                .unwrap_or(now_us);

            append_brain_event_on_writer(
                writer,
                &namespace_for_op,
                &profile_id,
                &event_kind,
                &payload,
                commit_at_us,
            )
            .await
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "brain_persist_append_event",
                    e,
                )
            })?;
            let snapshot_json = proposed.to_snapshot_json().map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "brain_persist_serialize_snapshot",
                    e,
                )
            })?;
            upsert_snapshot_json_on_writer(writer, &namespace_for_op, snapshot_json, commit_at_us)
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "brain_persist_upsert_snapshot",
                        e,
                    )
                })?;
            Ok(Box::new(MutationOutcome::Committed {
                result,
                state: Box::new(proposed),
                snapshot_version: commit_at_us,
            }) as Box<dyn std::any::Any + Send>)
        })
    });

    let boxed = sql.atomic_unit(op).await?;
    let outcome = *boxed
        .downcast::<MutationOutcome<R>>()
        .expect("persist_brain_state_mutation must return MutationOutcome");

    match outcome {
        MutationOutcome::Rejected(error) => Err(error),
        MutationOutcome::Committed {
            result,
            state: proposed,
            snapshot_version,
        } => {
            let mut t = tracker.lock().unwrap();
            if !matches!(t.snapshot_version(&namespace), Some(current) if current > snapshot_version)
            {
                *state.lock().unwrap() = *proposed;
                t.mark_loaded_at(namespace, Some(snapshot_version));
            }
            Ok(result)
        }
    }
}

async fn load_latest_snapshot_on_reader<R: SqlReader + ?Sized>(
    reader: &mut R,
    namespace: &str,
    entity_capacity: usize,
) -> Result<Option<(BrainStateSnapshot, i64)>, RuntimeError> {
    let row = reader
        .query_row(SqlStatement {
            sql: sql!("brain_profile_snapshot_latest").into(),
            params: vec![
                SqlValue::Text(SNAPSHOT_PROFILE_ID.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("brain_snapshot_load".into()),
        })
        .await?;

    match row {
        None => Ok(None),
        Some(row) => {
            let json_str = match row.get("snapshot_json") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => {
                    return Err(sql_err(
                        "brain persistence load snapshot",
                        "missing snapshot_json column",
                    ))
                }
            };
            let updated_at = match row.get("updated_at") {
                Some(SqlValue::Integer(n)) => *n,
                _ => {
                    return Err(sql_err(
                        "brain persistence load snapshot",
                        "missing updated_at column",
                    ))
                }
            };
            let snapshot: BrainStateSnapshot = serde_json::from_str(&json_str)
                .map_err(|e| sql_err("brain persistence deserialize snapshot", e))?;
            validate_brain_state_snapshot_with_capacity(&snapshot, entity_capacity)
                .map_err(|e| sql_err("brain persistence snapshot invariant violation", e))?;
            Ok(Some((snapshot, updated_at)))
        }
    }
}

pub async fn load_latest_snapshot(
    sql: &dyn SqlAccess,
    namespace: &str,
    entity_capacity: usize,
) -> Result<Option<(BrainStateSnapshot, i64)>, RuntimeError> {
    let mut reader = sql.reader().await?;
    load_latest_snapshot_on_reader(reader.as_mut(), namespace, entity_capacity).await
}

async fn load_snapshot_version_on_reader<R: SqlReader + ?Sized>(
    reader: &mut R,
    namespace: &str,
) -> Result<Option<i64>, RuntimeError> {
    let row = reader
        .query_scalar(SqlStatement {
            sql: sql!("brain_profile_snapshot_version").into(),
            params: vec![
                SqlValue::Text(SNAPSHOT_PROFILE_ID.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("brain_snapshot_version".into()),
        })
        .await?;

    match row {
        None => Ok(None),
        Some(SqlValue::Integer(version)) => Ok(Some(version)),
        other => Err(sql_err(
            "brain persistence load snapshot version",
            format!("expected integer updated_at, got {other:?}"),
        )),
    }
}

async fn load_snapshot_version(
    sql: &dyn SqlAccess,
    namespace: &str,
) -> Result<Option<i64>, RuntimeError> {
    let mut reader = sql.reader().await?;
    load_snapshot_version_on_reader(reader.as_mut(), namespace).await
}

// ── Replay-time policy exclusion (PR #1630 remediation) ─────────────────────

/// Start of the replay-time policy-exclusion window (inclusive), epoch µs:
/// 2026-05-01T00:00:00Z.
///
/// The defect fixed in PR #1630 caused `brain.auto_feedback` calls with an
/// ABSENT signal to default to `implicit_positive`, so the event log
/// accumulated a class of `FeedbackExplicit` rows that record the retriever's
/// own ranking rather than a judgment. The rows are retained in
/// `brain_event_log` for audit/telemetry; state rebuilds must not re-fold
/// them, hence the replay-time exclusion below.
const POLICY_EXCLUSION_WINDOW_START_US: i64 = 1_777_593_600_000_000;

/// End of the policy-exclusion window (exclusive), epoch µs.
///
/// Open-ended (`None`) DELIBERATELY, not as an oversight: deployed binaries
/// built before the PR #1630 fix keep minting class-member events, so any end
/// instant chosen before that fix is verifiably serving would be false. Pin
/// this bound to `Some(instant)` only when the deployment is verified by a
/// positive artifact (behavior only the fixed binary emits, with a pre-state
/// contrast), and cite that artifact in the pinning change — never a typed
/// date. The predicate below already honours the bound, so the pin is a
/// one-line change. Post-fix the handler abstains on omitted signals, so the
/// predicate goes naturally quiet rather than over-excluding.
const POLICY_EXCLUSION_WINDOW_END_US: Option<i64> = None;

/// The excluded signal spelling — the same canonical string that
/// `khive_brain_core::FeedbackEventKind::from_signal_str` accepts for the
/// `ImplicitPositive` kind.
const POLICY_EXCLUDED_SIGNAL: &str = "implicit_positive";

/// Per-row stderr detail is emitted for at most this many policy-excluded
/// rows per replay; the cohort can number in the thousands and the summary
/// line carries the authoritative counts. Quarantine rows are never capped.
const POLICY_EXCLUDED_LOG_DETAIL_CAP: usize = 10;

/// Why a replay row was skipped: unreadable/invalid (quarantine) vs readable
/// but excluded by policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySkipCategory {
    /// Structural or semantic validation failure (malformed JSON, invalid
    /// `section_signals`, missing payload column).
    Quarantine,
    /// Readable, valid row excluded by the replay-time policy filter
    /// (see `POLICY_EXCLUSION_WINDOW_START_US`, private to this module).
    PolicyExcluded,
}

/// Replay-time policy filter: `true` when `event` belongs to the
/// miscalibrated implicit-positive class (PR #1630) and must not be re-folded
/// during a state rebuild.
///
/// All four arms must hold:
/// 1. verb is `brain.feedback` — the `FeedbackExplicit` carrier
///    (`event.rs::interpret`);
/// 2. payload `signal` equals `implicit_positive` (the same canonical
///    spelling `FeedbackEventKind::from_signal_str` accepts);
/// 3. payload `served_by_profile_id` is present and non-empty — events with
///    no serving profile are the unaffected control population and MUST
///    replay;
/// 4. `created_at` lies in `[window_start_us, window_end_us)`; an end of
///    `None` means the window is open-ended.
///
/// Timestamp authority: the predicate reads the PAYLOAD's `created_at`, and
/// on the `brain.feedback` write path that is the same value as the row's
/// `created_at` column by construction — the handler stamps `commit_at_us`
/// into the event (`event.created_at = commit_at_us`), serializes THAT event
/// as the row payload, and passes the same `commit_at_us` as the column
/// value (see the `FeedbackEventWrite` arms and the
/// `append_brain_event_on_writer` call in this file). The two clocks cannot
/// diverge for the class this window describes.
///
/// The window is passed as parameters so tests can exercise both bounds
/// without touching the production constants.
fn is_policy_excluded(
    event: &khive_storage::event::Event,
    window_start_us: i64,
    window_end_us: Option<i64>,
) -> bool {
    if event.verb != "brain.feedback" {
        return false;
    }
    if event.payload.get("signal").and_then(|v| v.as_str()) != Some(POLICY_EXCLUDED_SIGNAL) {
        return false;
    }
    let has_serving_profile = event
        .payload
        .get("served_by_profile_id")
        .and_then(|v| v.as_str())
        .is_some_and(|profile_id| !profile_id.is_empty());
    if !has_serving_profile {
        return false;
    }
    event.created_at >= window_start_us
        && window_end_us.is_none_or(|end_us| event.created_at < end_us)
}

/// A single row skipped during replay (quarantined or policy-excluded), with
/// enough metadata to diagnose and re-examine the entry without re-running a
/// replay.
pub struct QuarantinedRow {
    /// Row primary key from `brain_event_log`.
    pub id: i64,
    /// Profile id recorded at write time (may be empty string if the column was null).
    pub profile_id: String,
    /// ISO-8601 / epoch-µs created_at value as recorded in the table.
    pub created_at: i64,
    /// Human-readable description of why the row was skipped.
    pub reason: String,
    /// Skip category: unreadable/invalid row (`Quarantine`) vs readable row
    /// excluded by policy (`PolicyExcluded`).
    pub category: ReplaySkipCategory,
    /// Leading ~200 chars of the raw payload for quick inspection (truncated with "…").
    pub payload_snippet: String,
}

/// Result of a replay load: valid events and the full skip manifest.
pub struct LoadEventsResult {
    /// Replayed events. `FeedbackUnjudged` telemetry rows are included without validation; a
    /// consumer that trains on these events must skip that kind, as the fold does.
    pub events: Vec<khive_storage::event::Event>,
    /// All rows skipped during replay — malformed/semantically invalid rows
    /// (quarantine) and readable rows excluded by policy — each tagged with
    /// its [`ReplaySkipCategory`]. The physical rows remain in
    /// `brain_event_log`; this vec makes them queryable.
    ///
    /// NOTE: because both categories share this vec, `quarantined.len()` is
    /// NOT the quarantine count — use [`Self::quarantine_count`] /
    /// [`Self::policy_excluded_count`]. (At the time this split landed the
    /// only consumers outside this module were tests; any new consumer must
    /// filter by category.)
    pub quarantined: Vec<QuarantinedRow>,
}

impl LoadEventsResult {
    /// Number of rows skipped due to structural or semantic validation failure.
    pub fn quarantine_count(&self) -> usize {
        self.count_category(ReplaySkipCategory::Quarantine)
    }

    /// Number of readable rows excluded by the replay-time policy filter.
    pub fn policy_excluded_count(&self) -> usize {
        self.count_category(ReplaySkipCategory::PolicyExcluded)
    }

    fn count_category(&self, category: ReplaySkipCategory) -> usize {
        self.quarantined
            .iter()
            .filter(|row| row.category == category)
            .count()
    }
}

pub async fn load_events_since(
    sql: &dyn SqlAccess,
    namespace: &str,
    since_us: i64,
) -> Result<LoadEventsResult, RuntimeError> {
    load_events_since_with_window(
        sql,
        namespace,
        since_us,
        POLICY_EXCLUSION_WINDOW_START_US,
        POLICY_EXCLUSION_WINDOW_END_US,
    )
    .await
}

/// Replay loader with the policy-exclusion window as parameters. The public
/// entry point above pins the production constants; this seam exists so the
/// `Some(end)` arm is integration-testable through the full row path today —
/// when the end bound is eventually pinned, the wiring is already exercised.
async fn load_events_since_with_window(
    sql: &dyn SqlAccess,
    namespace: &str,
    since_us: i64,
    window_start_us: i64,
    window_end_us: Option<i64>,
) -> Result<LoadEventsResult, RuntimeError> {
    let mut reader = sql.reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("brain_event_log_since").into(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(since_us),
            ],
            label: Some("brain_events_replay".into()),
        })
        .await?;

    let mut events = Vec::with_capacity(rows.len());
    let mut quarantined: Vec<QuarantinedRow> = Vec::new();
    // Running count of policy-excluded rows already pushed, kept beside the
    // vec so the per-row log-cap check is O(1) instead of rescanning
    // `quarantined` for every skipped row of a thousands-size cohort.
    let mut policy_excluded_seen: usize = 0;

    for row in &rows {
        let row_id = match row.get("id") {
            Some(SqlValue::Integer(i)) => *i,
            _ => 0,
        };
        let profile_id = match row.get("profile_id") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let created_at = match row.get("created_at") {
            Some(SqlValue::Integer(i)) => *i,
            _ => 0,
        };

        let mut push_quarantine = |category: ReplaySkipCategory,
                                   reason: String,
                                   payload_raw: &str| {
            let snippet = if payload_raw.len() > 200 {
                let end = payload_raw.floor_char_boundary(200);
                format!("{}…", &payload_raw[..end])
            } else {
                payload_raw.to_string()
            };
            let label = match category {
                ReplaySkipCategory::Quarantine => "quarantined",
                ReplaySkipCategory::PolicyExcluded => "policy-excluded",
            };
            // Quarantine rows are rare and indicate corruption — always loud.
            // Policy-excluded rows can number in the thousands (open-ended
            // window over a historical cohort), so per-row detail stops after
            // the first few; the end-of-replay summary carries the full count.
            let log_row = category == ReplaySkipCategory::Quarantine
                || policy_excluded_seen < POLICY_EXCLUDED_LOG_DETAIL_CAP;
            if log_row {
                tracing::warn!(
                    "[brain] event-log replay: {label} row id={row_id} profile={profile_id:?}: {reason}"
                );
            } else if policy_excluded_seen == POLICY_EXCLUDED_LOG_DETAIL_CAP {
                tracing::warn!(
                    "[brain] event-log replay: further policy-excluded rows elided from per-row \
                     logging (cap {POLICY_EXCLUDED_LOG_DETAIL_CAP}); see the summary line for totals"
                );
            }
            if category == ReplaySkipCategory::PolicyExcluded {
                policy_excluded_seen += 1;
            }
            quarantined.push(QuarantinedRow {
                id: row_id,
                profile_id: profile_id.clone(),
                created_at,
                category,
                reason,
                payload_snippet: snippet,
            });
        };

        let payload_str = match row.get("payload") {
            Some(SqlValue::Text(s)) => s,
            _ => {
                push_quarantine(
                    ReplaySkipCategory::Quarantine,
                    "missing or non-text payload column".into(),
                    "",
                );
                continue;
            }
        };
        let event = match serde_json::from_str::<khive_storage::event::Event>(payload_str) {
            Ok(ev) => ev,
            Err(e) => {
                push_quarantine(
                    ReplaySkipCategory::Quarantine,
                    format!("malformed event JSON: {e}"),
                    payload_str,
                );
                continue;
            }
        };
        // Telemetry is retained without applying evidence validation or exclusion rules.
        if event.kind == EventKind::FeedbackUnjudged {
            events.push(event);
            continue;
        }
        if event.verb == "brain.section_feedback" {
            if let Err(error) = crate::section_feedback::decode_event(&event) {
                push_quarantine(
                    ReplaySkipCategory::Quarantine,
                    format!("invalid section feedback: {error}"),
                    payload_str,
                );
                continue;
            }
        }
        // Semantic validation: a brain.feedback row with an invalid section_signals
        // payload must be quarantined whole — before any posterior state mutation.
        // Same contract as the live handler, once retired section keys are dropped.
        if event.verb == "brain.feedback" {
            if let Some(ss) = event.payload.get("section_signals") {
                if let Err(e) = crate::validate_replayed_section_signals(ss) {
                    push_quarantine(
                        ReplaySkipCategory::Quarantine,
                        format!("semantically invalid section_signals: {e}"),
                        payload_str,
                    );
                    continue;
                }
            }
        }
        // Policy exclusion (PR #1630 remediation): readable, valid rows from
        // the miscalibrated implicit-positive window are skipped at replay
        // time. The physical row stays in brain_event_log (audit preserved);
        // it never reaches interpret/apply_signal.
        if is_policy_excluded(&event, window_start_us, window_end_us) {
            push_quarantine(
                ReplaySkipCategory::PolicyExcluded,
                "policy_excluded: implicit_positive feedback inside the PR #1630 window".into(),
                payload_str,
            );
            continue;
        }
        events.push(event);
    }
    let quarantine_count = quarantined
        .iter()
        .filter(|row| row.category == ReplaySkipCategory::Quarantine)
        .count();
    // Counted explicitly (not by subtraction) so a future third
    // ReplaySkipCategory variant cannot silently miscount this line.
    let policy_excluded_count = quarantined
        .iter()
        .filter(|row| row.category == ReplaySkipCategory::PolicyExcluded)
        .count();
    if !quarantined.is_empty() {
        tracing::warn!(
            "[brain] event-log replay: {} row(s) quarantined, {} row(s) policy-excluded \
             out of {} total; replayed {} clean event(s)",
            quarantine_count,
            policy_excluded_count,
            rows.len(),
            events.len()
        );
    }
    Ok(LoadEventsResult {
        events,
        quarantined,
    })
}

fn prepare_parked_state(mut state: BrainState, entity_capacity: usize) -> BrainState {
    let effective_capacity = entity_capacity.max(1);
    let normalize = |posterior: &mut khive_brain_core::BalancedRecallState| {
        if posterior.entity_posteriors.capacity() != effective_capacity {
            *posterior = khive_brain_core::BalancedRecallState::from_snapshot(
                posterior.to_snapshot(),
                entity_capacity,
            );
        }
    };
    normalize(&mut state.balanced_recall);
    for posterior in state.profile_states.values_mut() {
        normalize(posterior);
    }
    state
}

pub async fn ensure_loaded(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tracker: &Mutex<PersistenceTracker>,
    state: &Mutex<BrainState>,
    entity_capacity: usize,
) -> Result<(), RuntimeError> {
    let namespace = token.namespace().as_str().to_string();
    let sql = runtime.sql();
    let observed_version = load_snapshot_version(sql.as_ref(), &namespace).await?;

    {
        let t = tracker.lock().unwrap();
        if t.is_active(&namespace)
            && version_covers(t.snapshot_version(&namespace), observed_version)
        {
            return Ok(());
        }
    }

    let already_current = {
        let t = tracker.lock().unwrap();
        t.is_loaded(&namespace) && version_covers(t.snapshot_version(&namespace), observed_version)
    };

    let brain_state: Option<BrainState> = if already_current {
        None
    } else {
        let snapshot_result =
            load_latest_snapshot(sql.as_ref(), &namespace, entity_capacity).await?;

        let bs = if let Some((snapshot, updated_at)) = snapshot_result {
            let replay_result = load_events_since(sql.as_ref(), &namespace, updated_at).await?;

            let mut bs = BrainState::from_snapshot(snapshot, entity_capacity);

            for event in &replay_result.events {
                // Do not count telemetry or seed a serving profile's section state.
                if event.kind == EventKind::FeedbackUnjudged {
                    continue;
                }
                if event.verb == "brain.section_feedback" {
                    if let Err(error) = crate::section_feedback::replay(&mut bs, event) {
                        tracing::warn!(event_id = %event.id, %error, "section feedback replay skipped");
                    }
                    continue;
                }
                let signal = interpret(event);
                bs.balanced_recall.apply_signal(&signal);

                let serving_profile = event
                    .payload
                    .get("served_by_profile_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("balanced-recall-v1");

                {
                    let section_state =
                        crate::ensure_section_state_seeded(&mut bs.section_states, serving_profile);
                    section_state.apply_signal(&signal);
                }
            }

            crate::sync_balanced_recall_record(&mut bs);
            bs
        } else {
            BrainState::new(entity_capacity)
        };
        Some(bs)
    };

    // In test builds, honour the injected interleaving hook (if any).
    // The hook fires only for cold loads (brain_state.is_some() at this
    // point) so warm-path and already-loaded early-returns are unaffected.
    #[cfg(test)]
    if brain_state.is_some() {
        let hook = POST_LOAD_HOOK.lock().unwrap().take();
        if let Some(h) = hook {
            // Signal the test controller: "I am at the hook."
            let _ = h.reached_tx.send(());
            // Wait for the controller to give the go-ahead.
            let _ = h.proceed_rx.await;
        }
    }

    // Lock order: tracker → state (always in this sequence).
    // Reversing this order anywhere would risk deadlock; do not change.
    //
    // Publication is atomic: active_namespace, *state, and loaded_namespaces
    // are all updated inside a single tracker-held critical section.
    //
    // Re-check under the final guard: a concurrent loader may have completed
    // the same namespace while this task awaited the async DB load.  If so,
    // discard the stale brain_state we loaded and defer to the live slot.
    {
        let mut t = tracker.lock().unwrap();

        // Another loader or mutation may already have published this namespace
        // at the same or a newer durable generation while this task awaited DB
        // reads. Never replace that state with the older copy loaded above.
        if t.is_active(&namespace)
            && version_covers(t.snapshot_version(&namespace), observed_version)
        {
            return Ok(());
        }

        // The same rule applies when another task published the namespace into
        // saved_states rather than the active slot.
        let fresh_brain_state = if t.is_loaded(&namespace)
            && version_covers(t.snapshot_version(&namespace), observed_version)
        {
            None
        } else {
            brain_state
        };

        let current_ns = t.active_namespace.clone();

        // Snapshot readers also take this guard. Keep it from the outgoing
        // move through incoming publication so they never see the placeholder.
        let mut current_state = state.lock().unwrap();
        let new_state = if let Some(ref from_ns) = current_ns {
            let saved_current = prepare_parked_state(
                std::mem::replace(&mut *current_state, BrainState::new(entity_capacity)),
                entity_capacity,
            );
            #[cfg(test)]
            namespace_move_tests::pause_after_move(state);
            let restored = t.swap_namespace(from_ns, saved_current, namespace.clone());
            fresh_brain_state
                .or(restored)
                .unwrap_or_else(|| BrainState::new(entity_capacity))
        } else {
            fresh_brain_state.unwrap_or_else(|| BrainState::new(entity_capacity))
        };

        // Drain any hook signals that arrived before this load completed.
        // These are signals queued by `route_signal` while the namespace was
        // still cold (no prior `ensure_loaded`).  Applying them here, after the
        // snapshot + replay path, preserves ordering:
        //   persisted snapshot → replayed events → queued hook signals
        let pending = t.drain_pending_signals(&namespace);
        let mut final_state = new_state;
        for sig in &pending {
            crate::apply_dispatch_signal(&mut final_state, sig);
        }

        // Write the new state while the tracker lock is still held.
        // After this line active_namespace, *state, and loaded_namespaces are
        // all consistent; no concurrent dispatch can observe a partial view.
        *current_state = final_state;

        t.mark_loaded_at(namespace, observed_version);
    }

    Ok(())
}

// ── BRAIN-007: event-log replay quarantine diagnostics ────────────────────────

#[cfg(test)]
#[path = "persist/brain_007_replay_quarantine_tests.rs"]
mod brain_007_replay_quarantine;

// ── PR #1630 remediation: replay-time policy exclusion of the miscalibrated
//    implicit-positive window ─────────────────────────────────────────────────

#[cfg(test)]
#[path = "persist/replay_policy_exclusion_tests.rs"]
mod replay_policy_exclusion;

// ── BRAINCORE-AUD-001: entity-posterior cache-capacity enforcement on load ────

#[cfg(test)]
#[path = "persist/braincore_aud_001_capacity_tests.rs"]
mod braincore_aud_001_capacity;

#[cfg(test)]
#[path = "persist/snapshot_copy_regression_tests.rs"]
mod snapshot_copy_regression;

// ── ADR-067 Fork C slice 2: persist_brain_state_mutation
// write-queue routing ──────────────────────────────────────────────────────

#[cfg(test)]
#[path = "persist/persist_write_queue_routing_tests.rs"]
mod persist_write_queue_routing;

#[cfg(test)]
mod namespace_move_tests;

#[cfg(test)]
mod storage_error_tests;
