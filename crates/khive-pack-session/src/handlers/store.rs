//! `session.store` — store a session record as a `kind=session` note.

use serde::Deserialize;
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, render_session_full};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreParams {
    content: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Value>,
}

/// Store a session blob as a `kind=session` note.
///
/// Builds `properties` from the optional `metadata` object (if provided),
/// then overlays `agent_id` and `tags` from their explicit params so that
/// named params always win over metadata keys.
///
/// Implementation: `runtime.create_note(token, "session", None, content, None, props, vec![])`.
pub(crate) async fn handle_store(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: StoreParams = deser(params)?;

    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "session.store: content must not be empty".into(),
        ));
    }

    // Build properties: start from caller-supplied metadata (if any), then
    // overlay the named params so explicit args always take precedence.
    let mut props = match p.metadata {
        Some(Value::Object(obj)) => Value::Object(obj),
        Some(_) => {
            return Err(RuntimeError::InvalidInput(
                "session.store: metadata must be a JSON object".into(),
            ))
        }
        None => json!({}),
    };

    let obj = props.as_object_mut().expect("props is object");
    if let Some(agent_id) = &p.agent_id {
        obj.insert("agent_id".into(), json!(agent_id));
    }
    if let Some(tags) = &p.tags {
        obj.insert("tags".into(), json!(tags));
    }

    let note = runtime
        .create_note(
            token,
            "session",
            None,
            &p.content,
            None,
            Some(props),
            vec![],
        )
        .await?;

    Ok(render_session_full(&note))
}
