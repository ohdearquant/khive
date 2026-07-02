//! `session.resume` - fetch one session's full content by UUID or short prefix.

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{
    deser, fetch_session_note, resolve_session_uuid, to_session_record, ResumeParams, ResumeResult,
};

const VERB: &str = "session.resume";

pub(crate) async fn handle_resume(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ResumeParams = deser(params)?;

    let uuid = resolve_session_uuid(runtime, token, &p.id, VERB).await?;
    let note = fetch_session_note(runtime, token, uuid, VERB).await?;

    let result = ResumeResult {
        ok: true,
        session: to_session_record(&note),
    };
    Ok(serde_json::to_value(result).expect("ResumeResult serializes"))
}

#[cfg(test)]
mod tests {
    use khive_runtime::{KhiveRuntime, Namespace};
    use serde_json::json;
    use uuid::Uuid;

    use super::handle_resume;

    #[tokio::test]
    async fn non_uuid_non_hex_id_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_resume(&rt, &token, json!({ "id": "not-an-id!" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert_eq!(
            msg,
            "session.resume: id must be a full UUID or 8+ hex prefix; \
             valid values: full UUID or 8+ hex prefix; got not-an-id!",
            "error must match ADR-083's byte-exact contract (display, not debug, formatting \
             of the caller-supplied id)",
        );
    }

    #[tokio::test]
    async fn hex_prefix_shape_accepted_but_not_found() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = handle_resume(&rt, &token, json!({ "id": "deadbeef" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert_eq!(
            msg,
            "session.resume: id prefix deadbeef matched no records; \
             valid values: full UUID or 8+ hex prefix",
            "an 8+ hex string must be accepted as short-prefix shape and routed to \
             prefix resolution, not rejected as malformed, with the id displayed unquoted",
        );
    }

    #[tokio::test]
    async fn wrong_note_kind_rejected() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let note = rt
            .core()
            .create_note(
                &token,
                "observation",
                None,
                "not a session",
                None,
                None,
                vec![],
            )
            .await
            .expect("create a non-session note");

        let err = handle_resume(&rt, &token, json!({ "id": note.id.to_string() }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert_eq!(
            msg,
            "session.resume: expected note kind \"session\"; valid note kind: session; \
             got observation",
            "error must name the actual note kind, displayed unquoted",
        );
    }

    #[tokio::test]
    async fn valid_uuid_not_found_returns_not_found() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let missing = Uuid::new_v4().to_string();

        let err = handle_resume(&rt, &token, json!({ "id": missing }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::NotFound(msg) = err else {
            panic!("expected NotFound, got {err:?}");
        };
        assert!(
            msg.contains("session not found"),
            "error must be a not-found, not a validation error; got: {msg}",
        );
    }
}
