//! `session.get` — fetch a single session record by UUID.

use std::str::FromStr;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, render_session_full};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetParams {
    id: String,
}

/// Fetch a single session record by UUID for replay or context injection.
///
/// Returns the full Note record (`id`, `kind`, `content`, `properties`,
/// `tags`, `created_at`). Returns `NotFound` if the session does not exist
/// or has been soft-deleted.
pub(crate) async fn handle_get(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: GetParams = deser(params)?;

    let uuid = Uuid::from_str(&p.id).map_err(|_| {
        RuntimeError::InvalidInput(format!("session.get: id must be a UUID; got {:?}", p.id))
    })?;

    let note = runtime
        .notes(token)?
        .get_note(uuid)
        .await
        .map_err(|e| RuntimeError::Internal(format!("get_note: {e}")))?
        .ok_or_else(|| RuntimeError::NotFound(format!("session not found: {}", p.id)))?;

    if note.kind != "session" {
        return Err(RuntimeError::InvalidInput(format!(
            "session.get: expected kind=\"session\", got {:?}",
            note.kind
        )));
    }
    if note.deleted_at.is_some() {
        return Err(RuntimeError::NotFound(format!("session deleted: {}", p.id)));
    }

    Ok(render_session_full(&note))
}
