//! `session.list` - list stored sessions, newest first.

use chrono::DateTime;
use serde_json::Value;

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

    let limit = match p.limit {
        None => DEFAULT_LIMIT,
        Some(l) if (1..=MAX_LIMIT).contains(&l) => l,
        Some(l) => {
            return Err(RuntimeError::InvalidInput(format!(
                "{VERB}: limit must be in 1..={MAX_LIMIT}; valid values: integers 1 through {MAX_LIMIT}; got {l}"
            )))
        }
    };
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
                p.since.as_deref().unwrap_or_default()
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
mod tests {
    use chrono::DateTime;
    use khive_runtime::{KhiveRuntime, Namespace};
    use serde_json::json;

    use super::handle_list;

    #[tokio::test]
    async fn limit_zero_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_list(&rt, &token, json!({ "limit": 0 }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("limit must be in 1..=200"),
            "error must name the limit-range violation; got: {msg}",
        );
    }

    #[tokio::test]
    async fn limit_over_max_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_list(&rt, &token, json!({ "limit": 201 }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("limit must be in 1..=200") && msg.contains("got 201"),
            "error must name the limit-range violation with the offending value; got: {msg}",
        );
    }

    #[tokio::test]
    async fn limit_min_boundary_accepted() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let result = handle_list(&rt, &token, json!({ "limit": 1 }))
            .await
            .expect("limit=1 is the lower boundary of the valid range");

        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["limit"], json!(1));
    }

    #[tokio::test]
    async fn limit_max_boundary_accepted() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let result = handle_list(&rt, &token, json!({ "limit": 200 }))
            .await
            .expect("limit=200 is the upper boundary of the valid range");

        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["limit"], json!(200));
    }

    #[tokio::test]
    async fn blank_provider_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_list(&rt, &token, json!({ "provider": "" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("provider must be a non-empty string when provided"),
            "error must name the blank-provider violation; got: {msg}",
        );
    }

    #[tokio::test]
    async fn blank_agent_id_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_list(&rt, &token, json!({ "agent_id": "  " }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("agent_id must be a non-empty string when provided"),
            "error must name the blank-agent-id violation; got: {msg}",
        );
    }

    #[tokio::test]
    async fn invalid_since_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_list(&rt, &token, json!({ "since": "yesterday" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(msg.contains("since must be an RFC 3339 timestamp"));
        assert!(msg.contains("yesterday"));
    }

    #[test]
    fn ceil_to_micros_preserves_exact_microsecond_boundary() {
        let value = DateTime::parse_from_rfc3339("2026-01-01T00:00:00.123456Z").unwrap();
        assert_eq!(super::ceil_to_micros(&value), 1_767_225_600_123_456);
    }

    #[test]
    fn ceil_to_micros_rounds_up_sub_microsecond_fraction() {
        let value = DateTime::parse_from_rfc3339("2026-01-01T00:00:00.1234561Z").unwrap();
        assert_eq!(super::ceil_to_micros(&value), 1_767_225_600_123_457);
    }

    #[tokio::test]
    async fn since_excludes_session_floored_to_an_earlier_microsecond() {
        use khive_storage::note::Note;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let created_at_micros: i64 = 1_767_225_600_123_456; // 2026-01-01T00:00:00.123456Z
        let mut note = Note::new("local", "session", "boundary session");
        note.created_at = created_at_micros;
        note.updated_at = created_at_micros;
        rt.core()
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("insert boundary session");

        // One tenth of a microsecond after the stored session: floors to the
        // same microsecond, but is strictly later, so it must exclude it.
        let result = handle_list(
            &rt,
            &token,
            json!({ "since": "2026-01-01T00:00:00.1234561Z" }),
        )
        .await
        .expect("list since");

        assert_eq!(
            result["sessions"].as_array().expect("sessions array").len(),
            0
        );
        assert_eq!(result["total"], 0);
    }
}
