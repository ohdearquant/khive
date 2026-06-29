//! `session.list` — list stored sessions, newest first.

use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, render_session_summary};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    since: Option<String>,
}

/// List stored sessions, newest first.
///
/// Fetches a broad window of `kind=session` notes and applies in-memory
/// filtering by `agent_id` and `since`, then sorts by `created_at DESC`
/// before applying `offset` and `limit` pagination.
pub(crate) async fn handle_list(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ListParams = deser(params)?;
    let limit = p.limit.unwrap_or(20).clamp(1, 200) as usize;
    let offset = p.offset.unwrap_or(0) as usize;

    // Parse the optional `since` filter into microseconds.
    let since_micros: Option<i64> = match p.since.as_deref() {
        None => None,
        Some(s) => {
            let dt = DateTime::parse_from_rfc3339(s).map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "session.list: since must be ISO-8601 (e.g. 2026-01-01T00:00:00Z); got {s:?}"
                ))
            })?;
            Some(dt.timestamp_micros())
        }
    };

    // Fetch a wide window to cover offset + limit + filtering headroom.
    let window = (offset as u32)
        .saturating_add(limit as u32)
        .saturating_add(500);
    let notes = runtime
        .list_notes(token, Some("session"), window, 0)
        .await?;

    // Filter, sort newest-first, then paginate.
    let mut filtered: Vec<&khive_storage::note::Note> = notes
        .iter()
        .filter(|n| n.deleted_at.is_none())
        .filter(|n| match p.agent_id.as_deref() {
            None => true,
            Some(want) => {
                n.properties
                    .as_ref()
                    .and_then(|p| p.get("agent_id"))
                    .and_then(|v| v.as_str())
                    == Some(want)
            }
        })
        .filter(|n| match since_micros {
            None => true,
            Some(since) => n.created_at >= since,
        })
        .collect();

    filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let result: Vec<Value> = filtered
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(render_session_summary)
        .collect();

    Ok(Value::Array(result))
}
