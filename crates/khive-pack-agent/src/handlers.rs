//! Verb handlers for the agent pack (ADR-142 §1).
//!
//! Every handler here is a thin wire-surface layer over the `AgentStore`
//! trait object the pack was constructed with: parameter validation,
//! `spawn_fingerprint` computation, and the lifecycle transition table live
//! here; the durable table itself is entirely the store's concern.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::agent_lifecycle::{apply_transition, spawn_fingerprint, Trigger};
use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::AgentStore;
use khive_types::{AgentRecord, AgentState, TerminalReason};

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

fn optional_str<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn state_str(state: AgentState) -> &'static str {
    match state {
        AgentState::Spawned => "spawned",
        AgentState::Running => "running",
        AgentState::Suspended => "suspended",
        AgentState::Terminal => "terminal",
    }
}

fn terminal_reason_str(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::Completed => "completed",
        TerminalReason::Failed => "failed",
        TerminalReason::Killed => "killed",
        TerminalReason::Abandoned => "abandoned",
        TerminalReason::HostRestart => "host_restart",
    }
}

fn record_to_json(record: &AgentRecord) -> Value {
    json!({
        "agent_id": record.agent_id,
        "state": state_str(record.state),
        "terminal_reason": record.terminal_reason.map(terminal_reason_str),
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

/// `agent.spawn` — required `provider`, `task`; optional `idempotency_key`,
/// `provider_session_id`, `checkpoint_session_id`. Success: `{ agent_id, state }`.
pub(crate) async fn handle_spawn(
    store: &Arc<dyn AgentStore>,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let provider = require_str(&params, "provider", "agent.spawn")?.to_string();
    let task = require_str(&params, "task", "agent.spawn")?.to_string();
    let idempotency_key = optional_str(&params, "idempotency_key").map(str::to_string);
    let provider_session_id = optional_str(&params, "provider_session_id").map(str::to_string);
    let checkpoint_session_id = optional_str(&params, "checkpoint_session_id").map(str::to_string);

    let owner_actor = token
        .actor()
        .binding_id()
        .unwrap_or("anonymous")
        .to_string();

    let fingerprint = spawn_fingerprint(
        &provider,
        &task,
        provider_session_id.as_deref(),
        checkpoint_session_id.as_deref(),
    );

    if let Some(key) = &idempotency_key {
        if let Some(existing) = store.find_by_idempotency(&owner_actor, key).await? {
            if existing.spawn_fingerprint == fingerprint {
                return Ok(json!({
                    "agent_id": existing.agent_id,
                    "state": state_str(existing.state),
                }));
            }
            return Err(RuntimeError::InvalidInput(format!(
                "agent.spawn: idempotency_key {key:?} was already used with different arguments"
            )));
        }
    }

    if let Some(psid) = &provider_session_id {
        if let Some(existing) = store
            .find_non_terminal_by_provider_session(&provider, psid)
            .await?
        {
            return Err(RuntimeError::InvalidInput(format!(
                "agent.spawn: provider_session_id {psid:?} for provider {provider:?} is \
                 already bound to non-terminal record {}",
                existing.agent_id
            )));
        }
    }

    let now = Utc::now().timestamp_micros();
    let record = AgentRecord {
        agent_id: Uuid::new_v4().to_string(),
        state: AgentState::Spawned,
        terminal_reason: None,
        provider,
        provider_session_id,
        checkpoint_session_id,
        checkpoint_cursor: None,
        owner_actor,
        // This pack is only reached through in-process registry dispatch — no
        // ADR-137-mapped connection is wired in here — so every record
        // carries the distinguished `native` context marker (ADR-142 §1,
        // "Persistent process record").
        owner_peer_class: "native".to_string(),
        owner_write_namespace: token.namespace().as_str().to_string(),
        owner_visible_namespaces: token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_string())
            .collect(),
        spawn_fingerprint: fingerprint,
        spawned_at: now,
        state_changed_at: now,
        idempotency_key,
    };

    store.insert(&record).await?;

    Ok(json!({
        "agent_id": record.agent_id,
        "state": state_str(record.state),
    }))
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
    let record = load(store, "agent.suspend", id).await?;

    let outcome = apply_transition(record.state, record.terminal_reason, Trigger::Suspend)
        .map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "agent.suspend: illegal transition from {} for agent_id {id:?}",
                state_str(e.from)
            ))
        })?;

    if outcome.changed {
        let now = Utc::now().timestamp_micros();
        store
            .update_state(id, outcome.state, outcome.terminal_reason, now)
            .await?;
    }

    Ok(json!({
        "agent_id": record.agent_id,
        "state": state_str(outcome.state),
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
    let record = load(store, "agent.resume", id).await?;

    let outcome =
        apply_transition(record.state, record.terminal_reason, Trigger::Resume).map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "agent.resume: illegal transition from {} for agent_id {id:?}",
                state_str(e.from)
            ))
        })?;

    if outcome.changed {
        let now = Utc::now().timestamp_micros();
        store
            .update_state(id, outcome.state, outcome.terminal_reason, now)
            .await?;
    }

    Ok(json!({
        "agent_id": record.agent_id,
        "state": state_str(outcome.state),
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
    let record = load(store, "agent.kill", id).await?;

    let outcome =
        apply_transition(record.state, record.terminal_reason, Trigger::Kill).map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "agent.kill: illegal transition from {} for agent_id {id:?}",
                state_str(e.from)
            ))
        })?;

    if outcome.changed {
        let now = Utc::now().timestamp_micros();
        store
            .update_state(id, outcome.state, outcome.terminal_reason, now)
            .await?;
    }

    Ok(json!({
        "agent_id": record.agent_id,
        "state": state_str(outcome.state),
        "terminal_reason": outcome.terminal_reason.map(terminal_reason_str),
    }))
}
