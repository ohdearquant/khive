//! Brain state persistence — snapshot upsert, event-log append, namespace-scoped reload.

use std::collections::HashMap;
use std::sync::Mutex;

use khive_fold::Fold;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlAccess;
use serde_json::Value;

use khive_brain_core::{validate_brain_state_snapshot, BrainState, BrainStateSnapshot};

const SNAPSHOT_PROFILE_ID: &str = "__brain__";
const DEFAULT_SNAPSHOT_BATCH_SIZE: u64 = 5;

/// Tracks loaded namespaces and dirty event counts; owns the per-namespace state map.
pub struct PersistenceTracker {
    /// Namespace currently reflected in the shared `BrainState` slot, if any.
    active_namespace: Option<String>,
    /// Saved snapshots of in-memory state for namespaces that have been initialised
    /// but are not currently in the active slot.  Used for save-restore on switch.
    saved_states: HashMap<String, BrainState>,
    /// Namespaces for which state has been initialised (from DB or fresh default).
    loaded_namespaces: HashMap<String, ()>,
    dirty_counts: HashMap<String, u64>,
    snapshot_batch_size: u64,
}

impl Default for PersistenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistenceTracker {
    /// Create a new tracker with the default snapshot batch size.
    pub fn new() -> Self {
        Self {
            active_namespace: None,
            saved_states: HashMap::new(),
            loaded_namespaces: HashMap::new(),
            dirty_counts: HashMap::new(),
            snapshot_batch_size: DEFAULT_SNAPSHOT_BATCH_SIZE,
        }
    }

    /// Return `true` when the namespace has already been initialised in memory.
    pub fn is_loaded(&self, namespace: &str) -> bool {
        self.loaded_namespaces.contains_key(namespace)
    }

    /// Return `true` when `namespace` is the namespace currently in the shared slot.
    pub fn is_active(&self, namespace: &str) -> bool {
        self.active_namespace.as_deref() == Some(namespace)
    }

    /// Mark a namespace as initialised and set it as the active namespace.
    pub fn mark_loaded(&mut self, namespace: String) {
        self.loaded_namespaces.insert(namespace.clone(), ());
        self.active_namespace = Some(namespace);
    }

    /// Save the current active state into the per-namespace map, then mark a new
    /// namespace active.  Returns the saved state that should now be swapped back
    /// into the shared slot after `ensure_loaded` finishes.
    ///
    /// Callers pass in the current active `BrainState` (taken from the shared
    /// slot) and receive the saved state for `namespace` back (or `None` if it
    /// has never been swapped out — meaning the caller must initialise it fresh).
    pub fn swap_namespace(
        &mut self,
        from_namespace: &str,
        from_state: BrainState,
        to_namespace: String,
    ) -> Option<BrainState> {
        // Save current state.
        self.saved_states
            .insert(from_namespace.to_string(), from_state);
        // Retrieve the target namespace's saved state (if any).
        let saved = self.saved_states.remove(&to_namespace);
        self.active_namespace = Some(to_namespace);
        saved
    }

    /// Increment the dirty event counter and return `true` when a snapshot flush is due.
    pub fn increment_dirty(&mut self, namespace: &str) -> bool {
        let count = self.dirty_counts.entry(namespace.to_string()).or_insert(0);
        *count += 1;
        *count >= self.snapshot_batch_size
    }

    /// Reset the dirty counter after a successful snapshot flush.
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
            // BRAIN-AUD-002: validate numeric invariants before loading into live state.
            validate_brain_state_snapshot(&snapshot)
                .map_err(|e| sql_err("snapshot invariant violation", e))?;
            Ok(Some((snapshot, updated_at)))
        }
    }
}

pub async fn load_events_since(
    sql: &dyn SqlAccess,
    namespace: &str,
    since_us: i64,
) -> Result<Vec<khive_storage::event::Event>, RuntimeError> {
    let mut reader = sql.reader().await.map_err(|e| sql_err("reader", e))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT payload FROM brain_event_log WHERE namespace = ?1 AND created_at > ?2 ORDER BY created_at ASC, id ASC".into(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(since_us),
            ],
            label: Some("brain_events_replay".into()),
        })
        .await
        .map_err(|e| sql_err("load events", e))?;

    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        let payload_str = match row.get("payload") {
            Some(SqlValue::Text(s)) => s,
            _ => continue,
        };
        match serde_json::from_str::<khive_storage::event::Event>(payload_str) {
            Ok(event) => events.push(event),
            Err(_) => continue,
        }
    }
    Ok(events)
}

pub async fn ensure_loaded(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tracker: &Mutex<PersistenceTracker>,
    state: &Mutex<BrainState>,
    fold: &crate::fold::BalancedRecallFold,
    section_fold: &crate::fold::SectionPosteriorFold,
    entity_capacity: usize,
) -> Result<(), RuntimeError> {
    let namespace = token.namespace().as_str().to_string();

    // Fast path: the requested namespace is already active in the shared slot.
    {
        let t = tracker.lock().unwrap();
        if t.is_active(&namespace) {
            return Ok(());
        }
    }

    // BRAIN-AUD-001: namespace isolation via save-restore.
    //
    // When switching to a different namespace:
    //   1. Save the current shared state back into the per-namespace map.
    //   2. If the target namespace has a saved in-memory state, restore it.
    //   3. If the target namespace has never been initialised, load from DB
    //      (or initialise to a fresh default).
    //
    // This guarantees that no namespace ever sees another namespace's in-memory
    // state — each namespace operates on its own isolated BrainState.

    let already_loaded = {
        let t = tracker.lock().unwrap();
        t.is_loaded(&namespace)
    };

    let brain_state: Option<BrainState> = if already_loaded {
        // The target namespace was loaded before but is not currently active.
        // Its state is in saved_states — swap it back in below.
        None // signals: retrieve from saved_states via swap_namespace
    } else {
        // First time loading this namespace — initialise from DB or fresh default.
        let sql = runtime.sql();
        let snapshot_result = load_latest_snapshot(sql.as_ref(), &namespace).await?;

        let bs = if let Some((snapshot, updated_at)) = snapshot_result {
            let replay_events = load_events_since(sql.as_ref(), &namespace, updated_at).await?;

            let ctx = khive_fold::FoldContext::new();
            let mut bs = BrainState::from_snapshot(snapshot, entity_capacity);

            for event in &replay_events {
                let current = std::mem::replace(
                    &mut bs.balanced_recall,
                    khive_brain_core::BalancedRecallState::new(0),
                );
                bs.balanced_recall = fold.reduce(current, event, &ctx);

                let serving_profile = event
                    .payload
                    .get("served_by_profile_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("balanced-recall-v1");

                if let Some(section_state) = bs.section_states.remove(serving_profile) {
                    let updated = section_fold.reduce(section_state, event, &ctx);
                    bs.section_states
                        .insert(serving_profile.to_string(), updated);
                }
            }

            crate::sync_balanced_recall_record(&mut bs);
            bs
        } else {
            // No persisted snapshot — fresh default state for this namespace.
            BrainState::new(entity_capacity)
        };
        Some(bs)
    };

    // Atomically save current state + install the new namespace's state.
    let new_state = {
        let mut t = tracker.lock().unwrap();
        let current_ns = t.active_namespace.clone();
        let current_state = state.lock().unwrap();

        if let Some(from_ns) = current_ns {
            // Clone current state for saving; we'll swap below.
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

            // swap_namespace saves `saved_current` for from_ns and returns
            // the saved state for `namespace` (if any).
            let restored = t.swap_namespace(&from_ns, saved_current, namespace.clone());
            // If we computed a fresh state above, use that; otherwise use restored.
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
