//! `session.list` - list stored sessions, newest first.

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};

use super::{deser, require_non_empty_if_present, to_session_summary, ListParams, ListResult};
use crate::vocab::{DEFAULT_LIMIT, MAX_LIMIT, SESSION_KIND};

const VERB: &str = "session.list";

pub(crate) async fn handle_list(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ListParams = deser(params)?;
    require_non_empty_if_present(&p.provider, "provider", VERB)?;

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

    let filter = NoteFilter {
        kind: Some(SESSION_KIND.to_string()),
        property_filters,
        min_created_at: None,
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
}
