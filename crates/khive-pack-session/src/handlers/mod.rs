//! Verb handlers for the session pack, one file per verb.

// Serialization is not a dispatchable verb (no `HandlerDef` entry); `handle_export`
// is a forward-deployed in-process helper, kept until the export call site lands.
#[allow(dead_code)]
pub(crate) mod export;
pub(crate) mod get;
pub(crate) mod list;
pub(crate) mod store;

use khive_runtime::{micros_to_iso, RuntimeError};
use khive_storage::note::Note;
use serde_json::{json, Value};

/// Deserialize params from a `Value` into a typed struct.
pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// Compact session summary for list responses.
pub(crate) fn render_session_summary(note: &Note) -> Value {
    let props = note.properties.as_ref();
    let agent_id = props
        .and_then(|p| p.get("agent_id"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "id": note.id.as_hyphenated().to_string(),
        "kind": "session",
        "agent_id": agent_id,
        "created_at": micros_to_iso(note.created_at),
        "namespace": note.namespace,
    })
}

/// Full session envelope for store, get, and export responses.
pub(crate) fn render_session_full(note: &Note) -> Value {
    let props = note.properties.clone().unwrap_or_else(|| json!({}));
    let agent_id = props.get("agent_id").cloned().unwrap_or(Value::Null);
    let tags = props.get("tags").cloned().unwrap_or(Value::Null);
    json!({
        "id": note.id.as_hyphenated().to_string(),
        "kind": "session",
        "content": note.content,
        "agent_id": agent_id,
        "tags": tags,
        "properties": props,
        "created_at": micros_to_iso(note.created_at),
        "updated_at": micros_to_iso(note.updated_at),
        "namespace": note.namespace,
    })
}
