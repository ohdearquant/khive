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
