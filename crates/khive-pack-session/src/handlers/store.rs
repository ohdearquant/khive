//! `session.store` - store a session record as a `kind=session` note.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, require_non_empty_if_present, to_session_record, StoreParams, StoreResult};
use crate::vocab::SESSION_KIND;

const VERB: &str = "session.store";

pub(crate) async fn handle_store(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: StoreParams = deser(params)?;

    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{VERB}: content must not be empty"
        )));
    }
    require_non_empty_if_present(&p.title, "title", VERB)?;
    require_non_empty_if_present(&p.provider, "provider", VERB)?;
    require_non_empty_if_present(&p.provider_session_id, "provider_session_id", VERB)?;
    if let Some(tags) = &p.tags {
        for tag in tags {
            if tag.trim().is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{VERB}: tags entries must be non-empty strings"
                )));
            }
        }
    }

    let mut properties = serde_json::Map::new();
    if let Some(provider) = &p.provider {
        properties.insert("provider".into(), json!(provider));
    }
    if let Some(provider_session_id) = &p.provider_session_id {
        properties.insert("provider_session_id".into(), json!(provider_session_id));
    }
    if let Some(tags) = &p.tags {
        properties.insert("tags".into(), json!(tags));
    }

    let core = runtime.core();
    let note = core
        .create_note(
            token,
            SESSION_KIND,
            p.title.as_deref(),
            &p.content,
            None,
            Some(Value::Object(properties)),
            vec![],
        )
        .await?;

    let result = StoreResult {
        ok: true,
        session: to_session_record(&note),
    };
    Ok(serde_json::to_value(result).expect("StoreResult serializes"))
}

#[cfg(test)]
mod tests {
    use khive_runtime::{KhiveRuntime, Namespace};
    use serde_json::json;

    use super::handle_store;

    #[tokio::test]
    async fn empty_content_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_store(&rt, &token, json!({ "content": "" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("content must not be empty"),
            "error must name the empty-content violation; got: {msg}",
        );
    }

    #[tokio::test]
    async fn blank_title_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_store(&rt, &token, json!({ "content": "hello", "title": "" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("title must be a non-empty string when provided"),
            "error must name the blank-title violation; got: {msg}",
        );
    }

    #[tokio::test]
    async fn blank_provider_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_store(&rt, &token, json!({ "content": "hello", "provider": "" }))
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
    async fn blank_provider_session_id_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_store(
            &rt,
            &token,
            json!({ "content": "hello", "provider_session_id": "" }),
        )
        .await
        .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("provider_session_id must be a non-empty string when provided"),
            "error must name the blank-provider_session_id violation; got: {msg}",
        );
    }

    #[tokio::test]
    async fn empty_tag_entry_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_store(
            &rt,
            &token,
            json!({ "content": "hello", "tags": ["good", ""] }),
        )
        .await
        .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("tags entries must be non-empty strings"),
            "error must name the empty-tag-entry violation; got: {msg}",
        );
    }
}
