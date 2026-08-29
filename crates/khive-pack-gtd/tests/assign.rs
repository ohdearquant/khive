//! Tests for `gtd.assign` verb: task creation, validation, defaults, due dates.

mod common;

use chrono::{DateTime, NaiveDate};
use common::{assign, pack, rt, rt_with_timezone};
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

// ADR-169: date-only `due` anchors to the configured `[display] timezone`
// (not UTC), so `gtd.assign(due="2026-08-23")` stores the calendar date the
// caller typed regardless of which side of UTC the configured zone sits on.
// Assertions check the resulting calendar date (`.date_naive()` on the
// parsed offset datetime, never converted to UTC first) — never a
// hard-coded offset string, which would pass in one DST season and silently
// break in the other.

#[tokio::test]
async fn assign_due_date_only_anchors_to_configured_zone_west_of_utc() {
    let pack = pack(rt_with_timezone("America/New_York"));
    let resp = assign(&pack, json!({"title": "west of utc", "due": "2026-06-15"})).await;
    let due = resp["due"].as_str().expect("due must be a string");
    let parsed = DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));
    assert_eq!(
        parsed.date_naive(),
        NaiveDate::from_ymd_opt(2026, 6, 15).unwrap(),
        "calendar date must be the date the caller typed, not shifted by anchoring to UTC; got {due}"
    );
    assert!(
        parsed.offset().local_minus_utc() < 0,
        "America/New_York must anchor with a negative (west-of-UTC) offset; got {due}"
    );
}

#[tokio::test]
async fn assign_due_date_only_anchors_to_configured_zone_east_of_utc() {
    let pack = pack(rt_with_timezone("Asia/Tokyo"));
    let resp = assign(&pack, json!({"title": "east of utc", "due": "2026-06-15"})).await;
    let due = resp["due"].as_str().expect("due must be a string");
    let parsed = DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));
    assert_eq!(
        parsed.date_naive(),
        NaiveDate::from_ymd_opt(2026, 6, 15).unwrap(),
        "calendar date must be the date the caller typed, not shifted by anchoring to UTC; got {due}"
    );
    assert!(
        parsed.offset().local_minus_utc() > 0,
        "Asia/Tokyo must anchor with a positive (east-of-UTC) offset; got {due}"
    );
}

#[tokio::test]
async fn assign_due_date_only_anchors_correctly_across_a_dst_transition() {
    let pack = pack(rt_with_timezone("America/New_York"));

    // 2026-03-08 is the US spring-forward date (America/New_York EST->EDT);
    // 2026-03-09 is the day immediately after. Midnight itself is never
    // inside the 2am gap on either date, so both anchor unambiguously, but
    // the two calendar dates fall on opposite sides of the transition and
    // must resolve to different UTC offsets.
    let before = assign(&pack, json!({"title": "before dst", "due": "2026-03-08"})).await;
    let after = assign(&pack, json!({"title": "after dst", "due": "2026-03-09"})).await;

    let due_before = before["due"].as_str().expect("due must be a string");
    let due_after = after["due"].as_str().expect("due must be a string");
    let parsed_before = DateTime::parse_from_rfc3339(due_before)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due_before} — {e}"));
    let parsed_after = DateTime::parse_from_rfc3339(due_after)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due_after} — {e}"));

    assert_eq!(
        parsed_before.date_naive(),
        NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(),
        "got {due_before}"
    );
    assert_eq!(
        parsed_after.date_naive(),
        NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(),
        "got {due_after}"
    );
    assert_ne!(
        parsed_before.offset().local_minus_utc(),
        parsed_after.offset().local_minus_utc(),
        "the two dates straddle America/New_York's spring-forward transition and must \
         anchor to different UTC offsets (EST before, EDT after); got {due_before} and {due_after}"
    );
}

#[tokio::test]
async fn assign_due_with_explicit_offset_is_unaffected_by_configured_zone() {
    // A value that already carries an explicit offset keeps its existing UTC
    // normalization regardless of [display] timezone (ADR-169 scope: parse
    // sites only anchor date-ONLY input).
    let ny = pack(rt_with_timezone("America/New_York"));
    let tokyo = pack(rt_with_timezone("Asia/Tokyo"));
    let body = json!({"title": "explicit offset", "due": "2026-08-23T09:30:00-04:00"});

    let resp_ny = assign(&ny, body.clone()).await;
    let resp_tokyo = assign(&tokyo, body).await;

    let due_ny = resp_ny["due"].as_str().expect("due must be a string");
    let due_tokyo = resp_tokyo["due"].as_str().expect("due must be a string");
    assert_eq!(
        due_ny, due_tokyo,
        "an explicit-offset due must normalize identically regardless of the configured \
         display timezone"
    );

    let parsed = DateTime::parse_from_rfc3339(due_ny)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due_ny} — {e}"));
    assert_eq!(
        parsed,
        DateTime::parse_from_rfc3339("2026-08-23T09:30:00-04:00").unwrap(),
        "explicit-offset due must denote the same instant as written; got {due_ny}"
    );
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
async fn assign_due_survives_agent_mode_presentation_round_trip() {
    // The top-level `due` convenience field returned by gtd.assign (and
    // echoed by gtd.tasks/gtd.next) must survive Agent-mode presentation
    // verbatim, not get minute-truncated like a generic top-level timestamp.
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "ship release", "status": "next", "due": "2026-08-01T09:30:15-04:00"}),
    )
    .await;
    let due = resp["due"]
        .as_str()
        .expect("due must be a string")
        .to_string();
    chrono::DateTime::parse_from_rfc3339(&due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due}, error: {e}"));

    let presented_assign =
        khive_runtime::present(resp.clone(), khive_runtime::PresentationMode::Agent, 0);
    assert_eq!(
        presented_assign["due"],
        json!(due),
        "gtd.assign due must round-trip verbatim through Agent-mode presentation"
    );

    let tasks = pack
        .dispatch("gtd.tasks", json!({"status": "next"}))
        .await
        .expect("tasks ok");
    let presented_tasks = khive_runtime::present(tasks, khive_runtime::PresentationMode::Agent, 0);
    let via_tasks = presented_tasks
        .as_array()
        .expect("gtd.tasks response is an array")
        .iter()
        .find(|t| t["title"] == "ship release")
        .expect("assigned task present in gtd.tasks");
    assert_eq!(via_tasks["due"], json!(due));

    let next = pack.dispatch("gtd.next", json!({})).await.expect("next ok");
    let presented_next = khive_runtime::present(next, khive_runtime::PresentationMode::Agent, 0);
    let via_next = presented_next
        .as_array()
        .expect("gtd.next response is an array")
        .iter()
        .find(|t| t["title"] == "ship release")
        .expect("assigned task present in gtd.next");
    assert_eq!(via_next["due"], json!(due));
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
