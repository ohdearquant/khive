//! Duplicate judgments share the ordinary lifecycle decision and guarded write.

use super::{CompleteDecision, DependencyOptions, TransitionDecision};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::Note;
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::Value;
use uuid::Uuid;

async fn partner(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source: Uuid,
    target: &str,
    raw: Option<&str>,
) -> Result<Option<Uuid>, RuntimeError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if target != "cancelled" {
        return Err(RuntimeError::InvalidInput(
            "duplicate_of requires status=cancelled".into(),
        ));
    }
    let id = super::resolve_lifecycle_uuid(raw, runtime).await?;
    if id == source {
        return Err(RuntimeError::InvalidInput(
            "duplicate_of cannot name the task itself".into(),
        ));
    }
    let note = runtime
        .notes(token)?
        .get_note(id)
        .await?
        .filter(|note| note.deleted_at.is_none())
        .ok_or_else(|| RuntimeError::NotFound(format!("duplicate_of task {raw:?}")))?;
    if note.kind != "task" {
        return Err(RuntimeError::InvalidInput(
            "duplicate_of must name a task".into(),
        ));
    }
    Ok(Some(id))
}

/// Prepare an optional duplicate judgment without changing the legacy API.
pub async fn prepare_transition(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw_id: &str,
    raw_status: &str,
    note_arg: Option<&str>,
    options: DependencyOptions,
    duplicate_of: Option<&str>,
) -> Result<(TransitionDecision, Option<Uuid>), RuntimeError> {
    let mut decision =
        super::prepare_transition(runtime, token, raw_id, raw_status, note_arg, options).await?;
    let (note, target) = match &decision {
        TransitionDecision::NoOp { note, target, .. }
        | TransitionDecision::Write { note, target, .. } => (note, target),
    };
    let duplicate_of = partner(runtime, token, note.id, target, duplicate_of).await?;
    if let Some(id) = duplicate_of {
        let canonical = id.to_string();
        match &mut decision {
            TransitionDecision::NoOp { note, .. } => {
                if note
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("duplicate_of"))
                    .and_then(Value::as_str)
                    != Some(canonical.as_str())
                {
                    return Err(RuntimeError::InvalidInput("same-status cancellation requires the exact recorded duplicate_of; a new or changed judgment would be discarded".into()));
                }
            }
            TransitionDecision::Write { props, .. } => {
                props["duplicate_of"] = Value::String(canonical)
            }
        }
    }
    Ok((decision, duplicate_of))
}

/// Prepare cancellation and its duplicate reference as one property update.
pub async fn prepare_complete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw_id: &str,
    status_arg: Option<&str>,
    result_arg: Option<&str>,
    options: DependencyOptions,
    duplicate_of: Option<&str>,
) -> Result<(CompleteDecision, Option<Uuid>), RuntimeError> {
    let mut decision =
        super::prepare_complete(runtime, token, raw_id, status_arg, result_arg, options).await?;
    let duplicate_of = partner(
        runtime,
        token,
        decision.note.id,
        decision.target,
        duplicate_of,
    )
    .await?;
    if let Some(id) = duplicate_of {
        decision.props["duplicate_of"] = Value::String(id.to_string());
    }
    Ok((decision, duplicate_of))
}

fn validate_judgment(
    snapshot: &Note,
    target: &str,
    props: &Value,
    id: Uuid,
) -> Result<(), RuntimeError> {
    if target != "cancelled"
        || id == snapshot.id
        || props.get("duplicate_of").and_then(Value::as_str) != Some(id.to_string().as_str())
    {
        return Err(RuntimeError::InvalidInput(
            "invalid prepared duplicate cancellation judgment".into(),
        ));
    }
    Ok(())
}

fn require_partner(statement: &mut SqlStatement, id: Uuid) {
    let param = statement.params.len() + 1;
    statement.sql.push_str(&format!(
        " AND EXISTS (SELECT 1 FROM notes AS duplicate_partner WHERE duplicate_partner.id = ?{param} AND duplicate_partner.kind = 'task' AND duplicate_partner.deleted_at IS NULL)"
    ));
    statement.params.push(SqlValue::Text(id.to_string()));
}

/// Both cancellation paths and atomic v1 apply this same source/partner guard.
pub fn transition_statement(
    snapshot: &Note,
    expected_current: &str,
    target: &str,
    new_props: &Value,
    updated_at: i64,
    duplicate_of: Option<Uuid>,
) -> Result<SqlStatement, RuntimeError> {
    let mut statement =
        super::gtd_transition_statement(snapshot, expected_current, target, new_props, updated_at)?;
    if let Some(id) = duplicate_of {
        validate_judgment(snapshot, target, new_props, id)?;
        let param = statement.params.len() + 1;
        statement.sql.push_str(&format!(" AND version = ?{param}"));
        statement.params.push(SqlValue::Integer(snapshot.version));
        require_partner(&mut statement, id);
    }
    Ok(statement)
}

/// A recorded duplicate is an exact read assertion, never another write.
pub fn noop_assertion_statement(
    snapshot: &Note,
    expected_current: &str,
    duplicate_of: Option<Uuid>,
) -> Result<SqlStatement, RuntimeError> {
    let mut statement = super::gtd_noop_assertion_statement(snapshot, expected_current)?;
    if let Some(id) = duplicate_of {
        validate_judgment(
            snapshot,
            expected_current,
            snapshot.properties.as_ref().unwrap_or(&Value::Null),
            id,
        )?;
        let param = statement.params.len() + 1;
        statement.sql.push_str(&format!(" AND json_type(properties, '$.duplicate_of') = 'text' AND json_extract(properties, '$.duplicate_of') = ?{param}"));
        statement.params.push(SqlValue::Text(id.to_string()));
        require_partner(&mut statement, id);
    }
    Ok(statement)
}

pub(super) async fn assert_noop(
    runtime: &KhiveRuntime,
    snapshot: &Note,
    current: &str,
    duplicate_of: Option<Uuid>,
) -> Result<(), RuntimeError> {
    if duplicate_of.is_some() {
        let statement = noop_assertion_statement(snapshot, current, duplicate_of)?;
        let rows = runtime.sql().reader().await?.query_all(statement).await?;
        if rows.len() != 1 {
            return Err(RuntimeError::InvalidInput(
                "duplicate cancellation assertion changed after preparation".into(),
            ));
        }
    }
    Ok(())
}
