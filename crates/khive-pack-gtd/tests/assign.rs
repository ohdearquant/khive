//! Tests for `gtd.assign` verb: task creation, validation, defaults, due dates.

mod common;

use common::{assign, pack, rt};
use serde_json::json;

#[tokio::test]
async fn assign_creates_a_task_with_defaults() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "write README", "priority": "p1"})).await;
    assert_eq!(resp["kind"], "task");
    assert_eq!(resp["title"], "write README");
    assert_eq!(resp["status"], "inbox");
    assert_eq!(resp["priority"], "p1");
    assert!(resp["id"].as_str().unwrap().len() == 8);
    assert!(resp["full_id"].as_str().unwrap().contains('-'));
}

#[tokio::test]
async fn assign_rejects_empty_title() {
    let pack = pack(rt());
    let err = pack
        .dispatch("gtd.assign", json!({"title": "  "}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("title must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn assign_rejects_invalid_status_and_priority() {
    let pack = pack(rt());
    let err = pack
        .dispatch("gtd.assign", json!({"title": "x", "status": "bogus"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid status"));

    let err = pack
        .dispatch("gtd.assign", json!({"title": "x", "priority": "p9"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid priority"));
}

#[tokio::test]
async fn assign_alias_status_normalizes_to_canonical() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "ship feature", "status": "in_progress"}),
    )
    .await;
    assert_eq!(resp["status"], "active");
}

#[tokio::test]
async fn assign_rejects_terminal_status_done() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "terminal task", "status": "done"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot create task in terminal state"),
        "expected terminal-state rejection; got: {msg}"
    );
    assert!(
        msg.contains("done"),
        "error must name the bad status; got: {msg}"
    );
}

#[tokio::test]
async fn assign_rejects_terminal_status_cancelled() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "terminal task", "status": "cancelled"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot create task in terminal state"),
        "expected terminal-state rejection; got: {msg}"
    );
    assert!(
        msg.contains("cancelled"),
        "error must name the bad status; got: {msg}"
    );
}

#[tokio::test]
async fn assign_accepts_inbox_status() {
    let pack = pack(rt());
    let resp = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "inbox task", "status": "inbox"}),
        )
        .await
        .expect("inbox is a valid initial status");
    assert_eq!(resp["status"], "inbox");
}

#[tokio::test]
async fn assign_due_iso8601_full_accepted() {
    let pack = pack(rt());
    let resp = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "iso due", "due": "2026-06-01T00:00:00Z"}),
        )
        .await
        .expect("full ISO-8601 due must be accepted");
    let due = resp["due"].as_str().expect("due must be a string");
    chrono::DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));
}

#[tokio::test]
async fn assign_due_date_only_accepted() {
    let pack = pack(rt());
    let resp = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "date-only due", "due": "2026-06-01"}),
        )
        .await
        .expect("date-only due must be accepted");
    let due = resp["due"].as_str().expect("due must be a string");
    chrono::DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));
}

#[tokio::test]
async fn assign_due_free_text_rejected() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "vague due", "due": "tomorrow"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("due must be ISO-8601"),
        "expected ISO-8601 error; got: {msg}"
    );
    assert!(
        msg.contains("tomorrow"),
        "error must echo the bad value; got: {msg}"
    );
}

#[tokio::test]
async fn assign_due_natural_language_rejected() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "vague due", "due": "June 1st 2026"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("due must be ISO-8601"),
        "expected ISO-8601 error; got: {msg}"
    );
}
