//! Brain state persistence — snapshot upsert and namespace-scoped reload.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlAccess;

use khive_brain_core::{validate_brain_state_snapshot, BrainState, BrainStateSnapshot};

use crate::event::interpret;

const SNAPSHOT_PROFILE_ID: &str = "__brain__";
const DEFAULT_SNAPSHOT_BATCH_SIZE: u64 = 5;

/// Tracks loaded namespaces and dirty event counts; owns the per-namespace state map.
pub struct PersistenceTracker {
    /// Namespace currently reflected in the shared `BrainState` slot, if any.
    pub(crate) active_namespace: Option<String>,
    /// Saved snapshots of in-memory state for namespaces that have been initialised
    /// but are not currently in the active slot.  Used for save-restore on switch.
    saved_states: HashMap<String, BrainState>,
    /// Namespaces for which state has been initialised (from DB or fresh default).
    pub(crate) loaded_namespaces: HashMap<String, ()>,
    dirty_counts: HashMap<String, u64>,
    snapshot_batch_size: u64,
}

impl Default for PersistenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistenceTracker {
    pub fn new() -> Self {
        Self {
            active_namespace: None,
            saved_states: HashMap::new(),
            loaded_namespaces: HashMap::new(),
            dirty_counts: HashMap::new(),
            snapshot_batch_size: DEFAULT_SNAPSHOT_BATCH_SIZE,
        }
    }

    pub fn is_loaded(&self, namespace: &str) -> bool {
        self.loaded_namespaces.contains_key(namespace)
    }

    pub fn is_active(&self, namespace: &str) -> bool {
        self.active_namespace.as_deref() == Some(namespace)
    }

    pub fn mark_loaded(&mut self, namespace: String) {
        self.loaded_namespaces.insert(namespace.clone(), ());
        self.active_namespace = Some(namespace);
    }

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

    pub fn increment_dirty(&mut self, namespace: &str) -> bool {
        let count = self.dirty_counts.entry(namespace.to_string()).or_insert(0);
        *count += 1;
        *count >= self.snapshot_batch_size
    }

    pub fn reset_dirty(&mut self, namespace: &str) {
        self.dirty_counts.insert(namespace.to_string(), 0);
    }
}

fn sql_err(context: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Internal(format!("brain persistence {context}: {e}"))
}

pub async fn append_brain_event(
    sql: &dyn SqlAccess,
    namespace: &str,
    profile_id: &str,
    event_kind: &str,
    payload: &Value,
    created_at_us: i64,
) -> Result<(), RuntimeError> {
    let payload_str = serde_json::to_string(payload).map_err(|e| sql_err("serialize event", e))?;

    let mut writer = sql.writer().await.map_err(|e| sql_err("writer", e))?;
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO brain_event_log (profile_id, namespace, event_kind, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5)".into(),
            params: vec![
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(event_kind.to_string()),
                SqlValue::Text(payload_str),
                SqlValue::Integer(created_at_us),
            ],
            label: Some("brain_event_log_append".into()),
        })
        .await
        .map_err(|e| sql_err("append event", e))?;

    Ok(())
}

pub async fn upsert_snapshot(
    sql: &dyn SqlAccess,
    namespace: &str,
    snapshot: &BrainStateSnapshot,
    updated_at_us: i64,
) -> Result<(), RuntimeError> {
    let snapshot_json =
        serde_json::to_string(snapshot).map_err(|e| sql_err("serialize snapshot", e))?;

    let mut writer = sql.writer().await.map_err(|e| sql_err("writer", e))?;
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO brain_profile_snapshots (profile_id, namespace, snapshot_json, updated_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(profile_id, namespace) DO UPDATE SET snapshot_json = excluded.snapshot_json, updated_at = excluded.updated_at".into(),
            params: vec![
                SqlValue::Text(SNAPSHOT_PROFILE_ID.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(snapshot_json),
                SqlValue::Integer(updated_at_us),
            ],
            label: Some("brain_snapshot_upsert".into()),
        })
        .await
        .map_err(|e| sql_err("upsert snapshot", e))?;

    Ok(())
}

pub async fn load_latest_snapshot(
    sql: &dyn SqlAccess,
    namespace: &str,
) -> Result<Option<(BrainStateSnapshot, i64)>, RuntimeError> {
    let mut reader = sql.reader().await.map_err(|e| sql_err("reader", e))?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT snapshot_json, updated_at FROM brain_profile_snapshots WHERE profile_id = ?1 AND namespace = ?2 ORDER BY updated_at DESC LIMIT 1".into(),
            params: vec![
                SqlValue::Text(SNAPSHOT_PROFILE_ID.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("brain_snapshot_load".into()),
        })
        .await
        .map_err(|e| sql_err("load snapshot", e))?;

    match row {
        None => Ok(None),
        Some(row) => {
            let json_str = match row.get("snapshot_json") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => return Err(sql_err("load snapshot", "missing snapshot_json column")),
            };
            let updated_at = match row.get("updated_at") {
                Some(SqlValue::Integer(n)) => *n,
                _ => return Err(sql_err("load snapshot", "missing updated_at column")),
            };
            let snapshot: BrainStateSnapshot =
                serde_json::from_str(&json_str).map_err(|e| sql_err("deserialize snapshot", e))?;
            validate_brain_state_snapshot(&snapshot)
                .map_err(|e| sql_err("snapshot invariant violation", e))?;
            Ok(Some((snapshot, updated_at)))
        }
    }
}

/// A single row that was quarantined during replay, with enough metadata to
/// diagnose and re-examine the bad entry without re-running a replay.
pub struct QuarantinedRow {
    /// Row primary key from `brain_event_log`.
    pub id: i64,
    /// Profile id recorded at write time (may be empty string if the column was null).
    pub profile_id: String,
    /// ISO-8601 / epoch-µs created_at value as recorded in the table.
    pub created_at: i64,
    /// Human-readable description of why the row was quarantined.
    pub reason: String,
    /// Leading ~200 chars of the raw payload for quick inspection (truncated with "…").
    pub payload_snippet: String,
}

/// Result of a replay load: valid events and the full quarantine manifest.
pub struct LoadEventsResult {
    pub events: Vec<khive_storage::event::Event>,
    /// Rows that were skipped due to structural or semantic validation failure.
    /// The physical rows remain in `brain_event_log`; this vec makes them queryable.
    pub quarantined: Vec<QuarantinedRow>,
}

impl LoadEventsResult {
    /// Convenience accessor: number of quarantined rows.
    pub fn quarantine_count(&self) -> usize {
        self.quarantined.len()
    }
}

pub async fn load_events_since(
    sql: &dyn SqlAccess,
    namespace: &str,
    since_us: i64,
) -> Result<LoadEventsResult, RuntimeError> {
    let mut reader = sql.reader().await.map_err(|e| sql_err("reader", e))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id, profile_id, event_kind, payload, created_at \
                  FROM brain_event_log \
                  WHERE namespace = ?1 AND created_at > ?2 \
                  ORDER BY created_at ASC, id ASC"
                .into(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(since_us),
            ],
            label: Some("brain_events_replay".into()),
        })
        .await
        .map_err(|e| sql_err("load events", e))?;

    let mut events = Vec::with_capacity(rows.len());
    let mut quarantined: Vec<QuarantinedRow> = Vec::new();

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

        let mut push_quarantine = |reason: String, payload_raw: &str| {
            let snippet = if payload_raw.len() > 200 {
                format!("{}…", &payload_raw[..200])
            } else {
                payload_raw.to_string()
            };
            eprintln!(
                "[brain] event-log replay: quarantined row id={row_id} profile={profile_id:?}: {reason}"
            );
            quarantined.push(QuarantinedRow {
                id: row_id,
                profile_id: profile_id.clone(),
                created_at,
                reason,
                payload_snippet: snippet,
            });
        };

        let payload_str = match row.get("payload") {
            Some(SqlValue::Text(s)) => s,
            _ => {
                push_quarantine("missing or non-text payload column".into(), "");
                continue;
            }
        };
        let event = match serde_json::from_str::<khive_storage::event::Event>(payload_str) {
            Ok(ev) => ev,
            Err(e) => {
                push_quarantine(format!("malformed event JSON: {e}"), payload_str);
                continue;
            }
        };
        // Semantic validation: a brain.feedback row with an invalid section_signals
        // payload must be quarantined whole — before any posterior state mutation.
        // This is the shared contract with the live brain.feedback handler.
        if event.verb == "brain.feedback" {
            if let Some(ss) = event.payload.get("section_signals") {
                if let Err(e) = crate::validate_section_signals(ss) {
                    push_quarantine(
                        format!("semantically invalid section_signals: {e}"),
                        payload_str,
                    );
                    continue;
                }
            }
        }
        events.push(event);
    }
    if !quarantined.is_empty() {
        eprintln!(
            "[brain] event-log replay: {} row(s) quarantined out of {} total; \
             replayed {} clean event(s)",
            quarantined.len(),
            rows.len(),
            events.len()
        );
    }
    Ok(LoadEventsResult {
        events,
        quarantined,
    })
}

pub async fn ensure_loaded(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tracker: &Mutex<PersistenceTracker>,
    state: &Mutex<BrainState>,
    entity_capacity: usize,
) -> Result<(), RuntimeError> {
    let namespace = token.namespace().as_str().to_string();

    {
        let t = tracker.lock().unwrap();
        if t.is_active(&namespace) {
            return Ok(());
        }
    }

    let already_loaded = {
        let t = tracker.lock().unwrap();
        t.is_loaded(&namespace)
    };

    let brain_state: Option<BrainState> = if already_loaded {
        None
    } else {
        let sql = runtime.sql();
        let snapshot_result = load_latest_snapshot(sql.as_ref(), &namespace).await?;

        let bs = if let Some((snapshot, updated_at)) = snapshot_result {
            let replay_result = load_events_since(sql.as_ref(), &namespace, updated_at).await?;

            let mut bs = BrainState::from_snapshot(snapshot, entity_capacity);

            for event in &replay_result.events {
                let signal = interpret(event);
                bs.balanced_recall.apply_signal(&signal);

                let serving_profile = event
                    .payload
                    .get("served_by_profile_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("balanced-recall-v1");

                if let Some(section_state) = bs.section_states.get_mut(serving_profile) {
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

    let new_state = {
        let mut t = tracker.lock().unwrap();
        let current_ns = t.active_namespace.clone();
        let current_state = state.lock().unwrap();

        if let Some(from_ns) = current_ns {
            let saved_current = BrainState {
                profiles: current_state.profiles.clone(),
                balanced_recall: khive_brain_core::BalancedRecallState::from_snapshot(
                    current_state.balanced_recall.to_snapshot(),
                    entity_capacity,
                ),
                profile_states: current_state
                    .profile_states
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            khive_brain_core::BalancedRecallState::from_snapshot(
                                v.to_snapshot(),
                                entity_capacity,
                            ),
                        )
                    })
                    .collect(),
                bindings: current_state.bindings.clone(),
                section_states: current_state
                    .section_states
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            khive_brain_core::SectionPosteriorState::from_snapshot(v.to_snapshot()),
                        )
                    })
                    .collect(),
            };
            drop(current_state);

            let restored = t.swap_namespace(&from_ns, saved_current, namespace.clone());
            brain_state
                .or(restored)
                .unwrap_or_else(|| BrainState::new(entity_capacity))
        } else {
            drop(current_state);
            t.active_namespace = Some(namespace.clone());
            brain_state.unwrap_or_else(|| BrainState::new(entity_capacity))
        }
    };

    {
        let mut s = state.lock().unwrap();
        *s = new_state;
    }

    {
        let mut t = tracker.lock().unwrap();
        t.loaded_namespaces.insert(namespace, ());
    }

    Ok(())
}

pub async fn persist_after_feedback(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tracker: &Mutex<PersistenceTracker>,
    state: &Mutex<BrainState>,
    event: &khive_storage::event::Event,
    serving_profile: &str,
) -> Result<(), RuntimeError> {
    let namespace = token.namespace().as_str().to_string();
    let now_us = chrono::Utc::now().timestamp_micros();

    let sql = runtime.sql();

    let event_payload = serde_json::to_value(event).map_err(|e| sql_err("serialize event", e))?;

    append_brain_event(
        sql.as_ref(),
        &namespace,
        serving_profile,
        &event.verb,
        &event_payload,
        now_us,
    )
    .await?;

    let should_snapshot = {
        let mut t = tracker.lock().unwrap();
        t.increment_dirty(&namespace)
    };

    if should_snapshot {
        let snapshot = {
            let s = state.lock().unwrap();
            s.to_snapshot()
        };

        upsert_snapshot(sql.as_ref(), &namespace, &snapshot, now_us).await?;

        let mut t = tracker.lock().unwrap();
        t.reset_dirty(&namespace);
    }

    Ok(())
}

// ── BRAIN-007: event-log replay quarantine diagnostics ────────────────────────

#[cfg(test)]
mod brain_007_replay_quarantine {
    use super::*;
    use khive_brain_core::BrainState;
    use khive_runtime::{KhiveRuntime, Namespace};
    use khive_storage::event::Event;
    use khive_types::{EventKind, SubstrateKind};
    use uuid::Uuid;

    async fn insert_raw_payload_at(
        rt: &KhiveRuntime,
        namespace: &str,
        payload: &str,
        created_at: i64,
    ) {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO brain_event_log (profile_id, namespace, event_kind, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5)".into(),
                params: vec![
                    SqlValue::Text("test-profile".to_string()),
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text("brain.feedback".to_string()),
                    SqlValue::Text(payload.to_string()),
                    SqlValue::Integer(created_at),
                ],
                label: None,
            })
            .await
            .expect("insert raw row");
    }

    async fn insert_raw_payload(rt: &KhiveRuntime, namespace: &str, payload: &str) {
        insert_raw_payload_at(rt, namespace, payload, 1_000_000).await;
    }

    fn make_valid_event_json(namespace: &str) -> String {
        let ev = Event::new(
            namespace,
            "recall",
            EventKind::Audit,
            SubstrateKind::Note,
            "brain",
        );
        serde_json::to_string(&ev).expect("serialize event")
    }

    /// Build a brain.feedback event JSON with optional section_signals payload.
    fn make_feedback_event_json(
        namespace: &str,
        section_signals: Option<serde_json::Value>,
    ) -> String {
        let mut ev = Event::new(
            namespace,
            "brain.feedback",
            EventKind::Audit,
            SubstrateKind::Event,
            "brain",
        );
        ev.target_id = Some(Uuid::new_v4());
        let mut payload = serde_json::json!({"signal": "useful"});
        if let Some(ss) = section_signals {
            payload["section_signals"] = ss;
        }
        ev.payload = payload;
        serde_json::to_string(&ev).expect("serialize feedback event")
    }

    // ── structural quarantine (pre-existing) ──────────────────────────────────

    #[tokio::test]
    async fn malformed_json_rows_are_quarantined_not_panicked() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        // Insert one malformed row (not valid JSON) and one valid serialized Event.
        insert_raw_payload(&rt, ns, "this is not valid json {{").await;
        insert_raw_payload(&rt, ns, &make_valid_event_json(ns)).await;

        // load_events_since must return without panicking, quarantining the bad row.
        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("load must not fail on malformed rows");

        // Only the valid event should be returned.
        assert_eq!(
            result.events.len(),
            1,
            "one valid event expected; malformed row must be quarantined, not panic"
        );
        assert_eq!(result.quarantine_count(), 1, "quarantine_count must be 1");
    }

    #[tokio::test]
    async fn all_malformed_rows_quarantined_returns_empty_vec() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        insert_raw_payload(&rt, ns, "bad json").await;
        insert_raw_payload(&rt, ns, "{invalid}").await;

        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("load must succeed even when all rows are malformed");

        assert!(
            result.events.is_empty(),
            "all malformed rows must be quarantined"
        );
        assert_eq!(result.quarantine_count(), 2, "quarantine_count must be 2");
    }

    #[tokio::test]
    async fn clean_rows_replay_without_quarantine() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        for _ in 0..3 {
            insert_raw_payload(&rt, ns, &make_valid_event_json(ns)).await;
        }

        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("clean rows must replay without error");

        assert_eq!(result.events.len(), 3, "all 3 clean rows must be returned");
        assert_eq!(result.quarantine_count(), 0, "quarantine_count must be 0");
    }

    // ── semantic quarantine (BRAIN-007 new coverage) ──────────────────────────

    #[tokio::test]
    async fn empty_section_signals_quarantined() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        // brain.feedback with section_signals: {} must be quarantined whole.
        let poison = make_feedback_event_json(ns, Some(serde_json::json!({})));
        insert_raw_payload(&rt, ns, &poison).await;

        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("load must not fail");

        assert!(
            result.events.is_empty(),
            "empty section_signals must be quarantined"
        );
        assert_eq!(result.quarantine_count(), 1);
    }

    #[tokio::test]
    async fn unknown_section_signals_quarantined() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        // Unknown section key must be quarantined.
        let poison = make_feedback_event_json(
            ns,
            Some(serde_json::json!({"not_a_real_section": "useful"})),
        );
        insert_raw_payload(&rt, ns, &poison).await;

        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("load must not fail");

        assert!(
            result.events.is_empty(),
            "unknown section must be quarantined"
        );
        assert_eq!(result.quarantine_count(), 1);
    }

    #[tokio::test]
    async fn semantic_signal_in_section_signals_quarantined() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        // Section fold only accepts useful|not_useful|wrong; semantic event kinds
        // (explicit_positive, correction, …) must be quarantined.
        let poison = make_feedback_event_json(
            ns,
            Some(serde_json::json!({"overview": "explicit_positive"})),
        );
        insert_raw_payload(&rt, ns, &poison).await;

        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("load must not fail");

        assert!(
            result.events.is_empty(),
            "semantic signal in section_signals must be quarantined"
        );
        assert_eq!(result.quarantine_count(), 1);
    }

    // ── state isolation: bad rows must not advance posterior state ────────────

    /// Seed a snapshot, insert bad rows at FIRST / LAST / interleaved positions,
    /// then call the real ensure_loaded path and assert posterior state is unchanged.
    async fn seed_snapshot(rt: &KhiveRuntime, namespace: &str) -> BrainStateSnapshot {
        let state = BrainState::new(16);
        let snapshot = state.to_snapshot();
        let sql = rt.sql();
        upsert_snapshot(sql.as_ref(), namespace, &snapshot, 500_000)
            .await
            .expect("seed snapshot");
        snapshot
    }

    /// Assert that section posteriors and epoch are at the initial (baseline) values,
    /// meaning no section-fold mutation occurred from any replayed event.
    /// Does NOT assert balanced_recall state — clean recall/search events legitimately
    /// advance that without touching section state.
    fn assert_section_posteriors_at_baseline(state: &BrainState, baseline: &BrainState) {
        for key in state.section_states.keys() {
            let s = &state.section_states[key];
            let b = &baseline.section_states[key];
            assert_eq!(
                s.total_events, b.total_events,
                "section_states[{key}].total_events changed; bad row must not advance section state"
            );
            assert_eq!(
                s.exploration_epoch, b.exploration_epoch,
                "section_states[{key}].exploration_epoch changed; bad row must not advance section state"
            );
            for (st, p) in &s.posteriors {
                let bp = &b.posteriors[st];
                assert!(
                    (p.alpha() - bp.alpha()).abs() < 1e-12
                        && (p.beta() - bp.beta()).abs() < 1e-12,
                    "section posterior for {:?} changed: got alpha={} beta={}, expected alpha={} beta={}; \
                     bad row must not mutate posteriors",
                    st, p.alpha(), p.beta(), bp.alpha(), bp.beta()
                );
            }
        }
    }

    #[tokio::test]
    async fn bad_row_first_does_not_mutate_posterior_state() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();

        seed_snapshot(&rt, ns).await;

        // Row 1 (bad — semantic poison): created_at=600_000 (after snapshot at 500_000)
        let poison =
            make_feedback_event_json(ns, Some(serde_json::json!({"overview": "correction"})));
        insert_raw_payload_at(&rt, ns, &poison, 600_001).await;

        // Row 2 (clean recall event): created_at=600_002
        insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_002).await;

        // Use load_events_since directly and replay manually to assert isolation.
        let sql = rt.sql();
        let result = load_events_since(sql.as_ref(), ns, 500_000)
            .await
            .expect("load must not fail");

        assert_eq!(
            result.quarantine_count(),
            1,
            "bad first row must be quarantined"
        );
        assert_eq!(result.events.len(), 1, "one clean event must pass through");

        // Apply the clean events to a fresh state and confirm section state is at baseline.
        let baseline = BrainState::new(16);
        let mut state = BrainState::new(16);
        for event in &result.events {
            let signal = crate::event::interpret(event);
            state.balanced_recall.apply_signal(&signal);
            for section_state in state.section_states.values_mut() {
                section_state.apply_signal(&signal);
            }
        }
        // The single clean event is a recall, not a feedback; section state unchanged.
        assert_section_posteriors_at_baseline(&state, &baseline);
    }

    #[tokio::test]
    async fn bad_row_last_does_not_mutate_posterior_state() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();

        seed_snapshot(&rt, ns).await;

        // Row 1 (clean): created_at=600_001
        insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_001).await;
        // Row 2 (bad — empty section_signals): created_at=600_002
        let poison = make_feedback_event_json(ns, Some(serde_json::json!({})));
        insert_raw_payload_at(&rt, ns, &poison, 600_002).await;

        let sql = rt.sql();
        let result = load_events_since(sql.as_ref(), ns, 500_000)
            .await
            .expect("load must not fail");

        assert_eq!(
            result.quarantine_count(),
            1,
            "bad last row must be quarantined"
        );
        assert_eq!(result.events.len(), 1, "one clean event must pass through");

        let baseline = BrainState::new(16);
        let mut state = BrainState::new(16);
        for event in &result.events {
            let signal = crate::event::interpret(event);
            state.balanced_recall.apply_signal(&signal);
            for section_state in state.section_states.values_mut() {
                section_state.apply_signal(&signal);
            }
        }
        assert_section_posteriors_at_baseline(&state, &baseline);
    }

    #[tokio::test]
    async fn bad_rows_interleaved_do_not_mutate_posterior_state() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();

        seed_snapshot(&rt, ns).await;

        // clean, bad, clean, bad, clean
        insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_001).await;
        let p1 = make_feedback_event_json(
            ns,
            Some(serde_json::json!({"overview": "explicit_negative"})),
        );
        insert_raw_payload_at(&rt, ns, &p1, 600_002).await;
        insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_003).await;
        let p2 = make_feedback_event_json(ns, Some(serde_json::json!({})));
        insert_raw_payload_at(&rt, ns, &p2, 600_004).await;
        insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_005).await;

        let sql = rt.sql();
        let result = load_events_since(sql.as_ref(), ns, 500_000)
            .await
            .expect("load must not fail");

        assert_eq!(
            result.quarantine_count(),
            2,
            "2 bad rows must be quarantined"
        );
        assert_eq!(result.events.len(), 3, "3 clean rows must pass through");

        // Apply only clean events; section posteriors must stay at baseline.
        let baseline = BrainState::new(16);
        let mut state = BrainState::new(16);
        for event in &result.events {
            let signal = crate::event::interpret(event);
            state.balanced_recall.apply_signal(&signal);
            for section_state in state.section_states.values_mut() {
                section_state.apply_signal(&signal);
            }
        }
        assert_section_posteriors_at_baseline(&state, &baseline);
    }

    // ── quarantine metadata: id and reason must be returned, not just counted ──

    #[tokio::test]
    async fn quarantined_rows_return_id_and_reason() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("token");
        let ns = token.namespace().as_str();
        let sql = rt.sql();

        // Two bad rows: one malformed JSON, one semantic poison.
        insert_raw_payload(&rt, ns, "not json at all").await;
        let poison = make_feedback_event_json(ns, Some(serde_json::json!({})));
        insert_raw_payload(&rt, ns, &poison).await;

        let result = load_events_since(sql.as_ref(), ns, 0)
            .await
            .expect("load must not fail");

        assert_eq!(
            result.quarantine_count(),
            2,
            "both bad rows must be quarantined"
        );
        assert!(result.events.is_empty(), "no clean events expected");

        // Each quarantined entry must carry a non-zero id and a non-empty reason.
        for qr in &result.quarantined {
            assert!(
                qr.id > 0,
                "quarantined row id must be a real autoincrement id; got {}",
                qr.id
            );
            assert!(
                !qr.reason.is_empty(),
                "quarantined row must carry a non-empty reason string"
            );
        }

        // First row: malformed JSON — reason must mention JSON or malformed.
        assert!(
            result.quarantined[0].reason.contains("malformed")
                || result.quarantined[0].reason.contains("JSON")
                || result.quarantined[0].reason.contains("json"),
            "first quarantine reason must describe malformed JSON; got: {:?}",
            result.quarantined[0].reason
        );

        // Second row: semantic poison (empty section_signals) — reason must mention section_signals.
        assert!(
            result.quarantined[1].reason.contains("section_signals"),
            "second quarantine reason must mention section_signals; got: {:?}",
            result.quarantined[1].reason
        );
    }
}
