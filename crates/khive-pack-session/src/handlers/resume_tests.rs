use khive_runtime::{KhiveRuntime, Namespace};
use serde_json::json;
use uuid::Uuid;

use super::handle_resume;

/// A runtime shaped like the pack's M2 configuration: bound to a
/// secondary `sessions` backend with the shared-graph main backend
/// reachable only through `core()`. Both backends are fresh in-memory
/// databases with migrations applied.
fn secondary_backend_runtime() -> KhiveRuntime {
    use std::sync::Arc;

    let make_backend = || {
        let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
        backend.pool().run_migrations().expect("migrations");
        Arc::new(backend)
    };
    let main_backend = make_backend();
    let sessions_backend = make_backend();

    let mut config = khive_runtime::RuntimeConfig::no_embeddings();
    config.packs = vec!["kg".to_string()];
    config.backend_id = khive_runtime::BackendId::parse("sessions").expect("valid backend id");

    KhiveRuntime::from_backend(sessions_backend, config).with_core_backend(main_backend)
}

#[tokio::test]
async fn resume_finds_session_stored_through_core_on_secondary_backend() {
    let rt = secondary_backend_runtime();
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let stored = crate::handlers::store::handle_store(
        &rt,
        &token,
        json!({ "content": "transcript body", "title": "probe" }),
    )
    .await
    .expect("store succeeds");
    let id = stored["session"]["id"].as_str().expect("id").to_string();

    let resumed = handle_resume(&rt, &token, json!({ "id": id }))
        .await
        .expect("resume must find the session store just wrote (ADR-083 §4 core() seam)");
    assert_eq!(resumed["session"]["id"].as_str(), Some(id.as_str()));

    let exported =
        crate::handlers::export::handle_export(&rt, &token, json!({ "id": id, "format": "json" }))
            .await
            .expect("export shares the same core() seam and must also find it");
    assert_eq!(exported["session"]["id"].as_str(), Some(id.as_str()));
}

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
