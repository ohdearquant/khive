//! Brain state persistence — snapshot upsert and namespace-scoped reload.

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
            let replay_events = load_events_since(sql.as_ref(), &namespace, updated_at).await?;

            let mut bs = BrainState::from_snapshot(snapshot, entity_capacity);

            for event in &replay_events {
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

        // Re-check 1: another loader already published this namespace as active.
        // The shared state slot already contains the live state; return without
        // clobbering it with our stale cold-loaded copy.
        if t.is_active(&namespace) {
            return Ok(());
        }

        // Re-check 2: another loader completed a cold load for this namespace
        // (it is now in saved_states) while this task was awaiting the DB.
        // Our brain_state was derived from a snapshot that is now stale relative
        // to any mutations that occurred after that loader published.  Discard
        // it so the save-restore path below uses the live saved state instead.
        let fresh_brain_state = if t.is_loaded(&namespace) {
            None
        } else {
            brain_state
        };

        let current_ns = t.active_namespace.clone();

        // Clone the parts of the current shared state we need for save-restore,
        // then immediately release the state lock so we can re-acquire it for
        // the final write (Rust Mutexes are not reentrant).
        let new_state = {
            let current_state = state.lock().unwrap();

            if let Some(ref from_ns) = current_ns {
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
                                khive_brain_core::SectionPosteriorState::from_snapshot(
                                    v.to_snapshot(),
                                ),
                            )
                        })
                        .collect(),
                };
                // Release the state guard before mutating the tracker (save-restore
                // path) so the re-acquire below cannot deadlock.
                drop(current_state);

                let restored = t.swap_namespace(from_ns, saved_current, namespace.clone());
                fresh_brain_state
                    .or(restored)
                    .unwrap_or_else(|| BrainState::new(entity_capacity))
            } else {
                drop(current_state);
                // No active namespace yet — first load.  active_namespace is set
                // below together with the state write and loaded_namespaces mark.
                fresh_brain_state.unwrap_or_else(|| BrainState::new(entity_capacity))
            }
        };

        // Write the new state while the tracker lock is still held.
        // After this line active_namespace, *state, and loaded_namespaces are
        // all consistent; no concurrent dispatch can observe a partial view.
        *state.lock().unwrap() = new_state;

        // For the no-active-namespace (first-load) path swap_namespace was not
        // called, so we set active_namespace here before marking loaded.
        if current_ns.is_none() {
            t.active_namespace = Some(namespace.clone());
        }

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
