//! `session.list` — list stored sessions, newest first.

use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

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
/// Delegates `agent_id` and `since` filtering to the query layer via
/// [`NoteFilter`], so completeness is independent of table size. Pagination
/// (`offset`, `limit`) is likewise pushed to the storage layer.
pub(crate) async fn handle_list(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ListParams = deser(params)?;
    let limit = p.limit.unwrap_or(20).clamp(1, 200) as u32;
    let offset = p.offset.unwrap_or(0) as u64;

    // Parse the optional `since` filter into microseconds for the DB predicate.
    let min_created_at: Option<i64> = match p.since.as_deref() {
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

    // Push agent_id as a JSON property predicate; since as a created_at column
    // bound. Both land in SQL — no in-memory filtering or window heuristics.
    let mut property_filters = Vec::new();
    if let Some(ref agent_id) = p.agent_id {
        property_filters.push(PropertyFilter {
            json_path: "$.agent_id".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(agent_id.clone()),
        });
    }

    let filter = NoteFilter {
        kind: Some("session".to_string()),
        property_filters,
        min_created_at,
        ..Default::default()
    };

    let page = runtime
        .notes(token)?
        .query_notes_filtered(
            token.namespace().as_str(),
            &filter,
            PageRequest { offset, limit },
        )
        .await?;

    let result: Vec<Value> = page.items.iter().map(render_session_summary).collect();
    Ok(Value::Array(result))
}
