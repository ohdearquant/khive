//! Session serialization — an in-process helper, NOT a dispatchable verb.
//!
//! `handle_export` serializes a session record for downstream use. It is called
//! directly in-process (no `HandlerDef`, no DSL dispatch arm): serialization is
//! a function, not a speech act. Forward-deployed until the in-process call site
//! lands; `#[allow(dead_code)]` on the module re-export covers the interim.

use std::str::FromStr;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, render_session_full};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    id: String,
    #[serde(default)]
    format: Option<String>,
}

/// Serialize a session record for downstream use.
///
/// `format="json"` (default) returns the full Note envelope as a JSON object.
/// `format="text"` returns the `content` field as a plain JSON string.
pub(crate) async fn handle_export(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ExportParams = deser(params)?;
    let format = p.format.as_deref().unwrap_or("json");

    if format != "json" && format != "text" {
        return Err(RuntimeError::InvalidInput(format!(
            "session.export: format must be \"json\" or \"text\"; got {format:?}"
        )));
    }

    let uuid = Uuid::from_str(&p.id).map_err(|_| {
        RuntimeError::InvalidInput(format!("session.export: id must be a UUID; got {:?}", p.id))
    })?;

    let note = runtime
        .notes(token)?
        .get_note(uuid)
        .await
        .map_err(|e| RuntimeError::Internal(format!("get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("session not found: {}", p.id)))?;

    if note.kind != "session" {
        return Err(RuntimeError::InvalidInput(format!(
            "session.export: expected kind=\"session\", got {:?}",
            note.kind
        )));
    }
    if note.deleted_at.is_some() {
        return Err(RuntimeError::NotFound(format!("session deleted: {}", p.id)));
    }

    match format {
        "text" => Ok(Value::String(note.content.clone())),
        _ => Ok(render_session_full(&note)),
    }
}
