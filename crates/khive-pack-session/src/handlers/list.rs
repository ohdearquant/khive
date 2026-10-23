//! `session.list` - list stored sessions, newest first.

use chrono::DateTime;
use serde_json::Value;

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

use super::{deser, require_non_empty_if_present, to_session_summary, ListParams, ListResult};
use crate::vocab::{DEFAULT_LIMIT, MAX_LIMIT, SESSION_KIND};

const VERB: &str = "session.list";

/// Round a parsed timestamp up to the nearest whole microsecond.
///
/// Storage records `created_at` at microsecond precision, but RFC 3339 permits
/// finer sub-microsecond fractions. Flooring (`timestamp_micros`) would let a
/// `since` value that falls strictly after a stored microsecond boundary still
/// match that boundary, silently admitting a session older than the requested
/// instant. Ceiling instead preserves the documented inclusive lower bound:
/// the resulting bound equals the boundary exactly when `since` lands on it,
/// and moves past it otherwise.
fn ceil_to_micros<Tz: chrono::TimeZone>(value: &DateTime<Tz>) -> i64 {
    let secs = value.timestamp();
    let subsec_nanos = i64::from(value.timestamp_subsec_nanos());
    secs * 1_000_000 + (subsec_nanos + 999) / 1000
}

pub(crate) async fn handle_list(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ListParams = deser(params)?;
    require_non_empty_if_present(&p.provider, "provider", VERB)?;
    require_non_empty_if_present(&p.agent_id, "agent_id", VERB)?;

    let limit = super::validate_limit(p.limit, DEFAULT_LIMIT, MAX_LIMIT).map_err(|l| {
        RuntimeError::InvalidInput(format!(
            "{VERB}: limit must be in 1..={MAX_LIMIT}; valid values: integers 1 through {MAX_LIMIT}; got {l}"
        ))
    })?;
    let offset = p.offset.unwrap_or(0) as u64;

    let mut property_filters = Vec::new();
    if let Some(provider) = &p.provider {
        property_filters.push(PropertyFilter {
            json_path: "$.provider".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(provider.clone()),
        });
    }
    if let Some(agent_id) = &p.agent_id {
        property_filters.push(PropertyFilter {
            json_path: "$.agent_id".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text(agent_id.clone()),
        });
    }

    let min_created_at = p
        .since
        .as_deref()
        .map(|raw| DateTime::parse_from_rfc3339(raw).map(|value| ceil_to_micros(&value)))
        .transpose()
        .map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "{VERB}: since must be an RFC 3339 timestamp; valid example: \
                 2026-01-01T00:00:00Z; got {:?}",
                mask_for_redaction_surface(
                    RedactionSurface::GateProbe,
                    p.since.as_deref().unwrap_or_default(),
                )
            ))
        })?;

    let filter = NoteFilter {
        kind: Some(SESSION_KIND.to_string()),
        property_filters,
        min_created_at,
        ..Default::default()
    };

    let core = runtime.core();
    let page = core
        .notes(token)?
        .query_notes_filtered(
            token.namespace().as_str(),
            &filter,
            PageRequest { offset, limit },
        )
        .await?;

    let sessions: Vec<_> = page.items.iter().map(to_session_summary).collect();
    let result = ListResult {
        ok: true,
        count: sessions.len(),
        sessions,
        total: page.total,
        limit,
        offset,
    };
    Ok(serde_json::to_value(result).expect("ListResult serializes"))
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
