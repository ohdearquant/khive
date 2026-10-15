use khive_runtime::{KhiveRuntime, Namespace};
use serde_json::json;
use uuid::Uuid;

use super::handle_export;

#[tokio::test]
async fn invalid_format_rejected_before_resolution() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    // `id` is also malformed; a resolution-first implementation would
    // fail with a different error. Getting the format error here proves
    // format validation runs before id resolution.
    let err = handle_export(&rt, &token, json!({ "id": "not-an-id!", "format": "xml" }))
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert_eq!(
        msg, "session.export: format must be one of [\"json\", \"markdown\"]; got xml",
        "format must be validated before id resolution, with the caller's value \
             displayed unquoted",
    );
}

#[tokio::test]
async fn json_format_accepted_proceeds_to_resolution() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let missing = Uuid::new_v4().to_string();

    let err = handle_export(&rt, &token, json!({ "id": missing, "format": "json" }))
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::NotFound(msg) = err else {
        panic!("expected NotFound (format accepted, id resolution reached), got {err:?}");
    };
    assert!(
        msg.contains("session not found"),
        "format=json must pass validation and reach id resolution; got: {msg}",
    );
}

#[tokio::test]
async fn markdown_format_accepted_proceeds_to_resolution() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let missing = Uuid::new_v4().to_string();

    let err = handle_export(&rt, &token, json!({ "id": missing, "format": "markdown" }))
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::NotFound(msg) = err else {
        panic!("expected NotFound (format accepted, id resolution reached), got {err:?}");
    };
    assert!(
        msg.contains("session not found"),
        "format=markdown must pass validation and reach id resolution; got: {msg}",
    );
}
