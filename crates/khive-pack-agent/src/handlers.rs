//! Verb handlers for the agent pack (ADR-142 §1).
//!
//! Every handler here is a thin wire-surface layer over the `AgentStore`
//! trait object the pack was constructed with: parameter validation,
//! and the lifecycle transition table live
//! here; the durable table itself is entirely the store's concern.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};

use khive_runtime::agent_lifecycle::{apply_transition, Transition, Trigger};
use khive_runtime::RuntimeError;
use khive_storage::AgentStore;
use khive_types::{AgentRecord, TerminalReason};

fn require_str<'a>(params: &'a Value, name: &str, verb: &str) -> Result<&'a str, RuntimeError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{verb} requires a non-empty string field \"{name}\""
            ))
        })
}

fn record_to_json(record: &AgentRecord) -> Value {
    json!({
        "agent_id": record.agent_id,
        "state": record.state.as_str(),
        "terminal_reason": record.terminal_reason.map(TerminalReason::as_str),
        "provider": record.provider,
        "provider_session_id": record.provider_session_id,
        "checkpoint_session_id": record.checkpoint_session_id,
        "checkpoint_cursor": record.checkpoint_cursor,
        "owner_actor": record.owner_actor,
        "owner_peer_class": record.owner_peer_class,
        "owner_write_namespace": record.owner_write_namespace,
        "owner_visible_namespaces": record.owner_visible_namespaces,
        "spawn_fingerprint": record.spawn_fingerprint,
        "spawned_at": record.spawned_at,
        "state_changed_at": record.state_changed_at,
        "idempotency_key": record.idempotency_key,
    })
}

async fn load(
    store: &Arc<dyn AgentStore>,
    verb: &str,
    id: &str,
) -> Result<AgentRecord, RuntimeError> {
    store
        .get(id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("{verb}: unknown agent_id {id:?}")))
}

async fn transition(
    store: &Arc<dyn AgentStore>,
    verb: &str,
    id: &str,
    trigger: Trigger,
) -> Result<(AgentRecord, Transition), RuntimeError> {
    let mut record = load(store, verb, id).await?;
    let mut attempts = 0;
    loop {
        let outcome =
            apply_transition(record.state, record.terminal_reason, trigger).map_err(|e| {
                RuntimeError::InvalidInput(format!(
                    "{verb}: illegal transition from {} for agent_id {id:?}",
                    e.from.as_str()
                ))
            })?;
        if !outcome.changed {
            return Ok((record, outcome));
        }
        if attempts == 3 {
            return Err(khive_types::KhiveError::conflict(format!(
                "{verb}: state changed concurrently; retry"
            ))
            .with_details(khive_types::Details::new_owned([
                ("reason", "state_changed_concurrently".into()),
                ("agent_id", id.to_owned()),
            ]))
            .into());
        }
        attempts += 1;
        if store
            .transition_state(
                id,
                record.state,
                outcome.state,
                outcome.terminal_reason,
                Utc::now().timestamp_micros(),
            )
            .await?
        {
            return Ok((record, outcome));
        }
        // Reapply the same sequential lifecycle rules to every missed CAS,
        // including the last attempt: a new no-op or refusal wins over retry.
        record = load(store, verb, id).await?;
    }
}

/// Refuse providers until a runtime adapter can actually start the process.
pub(crate) fn handle_spawn(params: Value) -> Result<Value, RuntimeError> {
    require_str(&params, "provider", "agent.spawn")?;
    require_str(&params, "task", "agent.spawn")?;
    Err(khive_types::KhiveError::unavailable("provider_unavailable")
        .with_details(khive_types::Details::new([(
            "reason",
            "provider_unavailable",
        )]))
        .into())
}

/// `agent.observe` — required `id`. Success: the full process-record field set.
pub(crate) async fn handle_observe(
    store: &Arc<dyn AgentStore>,
    params: Value,
) -> Result<Value, RuntimeError> {
    let id = require_str(&params, "id", "agent.observe")?;
    let record = load(store, "agent.observe", id).await?;
    Ok(record_to_json(&record))
}

/// `agent.suspend` — required `id`. Success: `{ agent_id, state, checkpoint_session_id }`.
///
/// Legal only from `running`; a no-op on an already-`suspended` record;
/// an illegal-transition error from `spawned` or `terminal`. This handler
/// does not perform the session-surface checkpoint write itself (no
/// checkpoint content is a parameter of this verb) — it reports the
/// record's currently stored `checkpoint_session_id` unchanged.
pub(crate) async fn handle_suspend(
    store: &Arc<dyn AgentStore>,
    params: Value,
) -> Result<Value, RuntimeError> {
    let id = require_str(&params, "id", "agent.suspend")?;
    let (record, outcome) = transition(store, "agent.suspend", id, Trigger::Suspend).await?;

    Ok(json!({
        "agent_id": record.agent_id,
        "state": outcome.state.as_str(),
        "checkpoint_session_id": record.checkpoint_session_id,
    }))
}

/// `agent.resume` — required `id`. Success: `{ agent_id, state }`.
///
/// A no-op on an already-`running` record; legal from `suspended`; an
/// illegal-transition error from `spawned` or `terminal`.
pub(crate) async fn handle_resume(
    store: &Arc<dyn AgentStore>,
    params: Value,
) -> Result<Value, RuntimeError> {
    let id = require_str(&params, "id", "agent.resume")?;
    let (record, outcome) = transition(store, "agent.resume", id, Trigger::Resume).await?;

    Ok(json!({
        "agent_id": record.agent_id,
        "state": outcome.state.as_str(),
    }))
}

/// `agent.kill` — required `id`. Success: `{ agent_id, state, terminal_reason }`.
///
/// Legal from `spawned`, `running`, or `suspended`; a no-op returning the
/// current state on an already-`terminal` record, never an error.
pub(crate) async fn handle_kill(
    store: &Arc<dyn AgentStore>,
    params: Value,
) -> Result<Value, RuntimeError> {
    let id = require_str(&params, "id", "agent.kill")?;
    let (record, outcome) = transition(store, "agent.kill", id, Trigger::Kill).await?;

    Ok(json!({
        "agent_id": record.agent_id,
        "state": outcome.state.as_str(),
        "terminal_reason": outcome.terminal_reason.map(TerminalReason::as_str),
    }))
}
