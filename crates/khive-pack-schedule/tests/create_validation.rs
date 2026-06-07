//! Tests for remind/schedule creation and input validation (C1, C3, C4, repeat).

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[tokio::test]
async fn remind_creates_pending_event() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "check status",
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("remind succeeds");

    assert!(result.get("id").is_some(), "remind returns id: {result}");
    assert_eq!(result["status"], "pending");
    assert_eq!(result["event_type"], "remind");
}

#[tokio::test]
async fn schedule_creates_pending_event_with_action() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"entity\", name=\"test\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule succeeds");

    assert!(result.get("id").is_some(), "schedule returns id: {result}");
    assert_eq!(result["event_type"], "schedule");
}

#[tokio::test]
async fn test_full_id_returns_36_char_schedule() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "check status", "at": "2099-06-01T09:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let id = result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id present");
    let full_id = result
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");

    assert_eq!(id.len(), 8, "id must be 8-char short prefix");
    assert_eq!(full_id.len(), 36, "full_id must be 36-char hyphenated UUID");
    assert!(
        full_id.starts_with(id),
        "full_id must start with the short id prefix"
    );
    assert!(full_id.contains('-'), "full_id must be hyphenated format");
}

// ── S-C1 regression: RFC 3339 timestamp validation ──────────────────────────

#[tokio::test]
async fn s_c1_schedule_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"test\")",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .expect("schedule with valid RFC 3339 must succeed");

    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_not_a_date() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"test\")",
                "at": "not-a-date"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "S-C1: error must reference RFC 3339 or the bad value; got: {msg}"
    );
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_natural_language() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"test\")",
                "at": "tomorrow at 3pm"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("RFC 3339") || err.to_string().contains("timestamp"),
        "S-C1: natural-language at must be rejected with RFC 3339 hint; got: {err}"
    );
}

#[tokio::test]
async fn s_c1_schedule_invalid_at_out_of_range_date() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"test\")",
                "at": "2027-13-99"
            }),
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("RFC 3339") || err.to_string().contains("timestamp"),
        "S-C1: out-of-range date must be rejected; got: {err}"
    );
}

#[tokio::test]
async fn s_c1_remind_invalid_at_is_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "hello",
                "at": "invalid"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("invalid"),
        "S-C1: remind with invalid at must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn s_c1_remind_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "morning standup",
                "at": "2099-06-15T09:00:00+00:00"
            }),
        )
        .await
        .expect("remind with valid RFC 3339 offset format must succeed");

    assert_eq!(result["status"], "pending");
}

// ── C3 regression: past dates rejected ───────────────────────────────────────

#[tokio::test]
async fn c3_schedule_past_date_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"past\")",
                "at": "2020-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("past") || msg.contains("future"),
        "C3: past at must be rejected with past/future hint; got: {msg}"
    );
}

#[tokio::test]
async fn c3_remind_past_date_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "stale reminder",
                "at": "2019-06-01T09:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("past") || msg.contains("future"),
        "C3: past remind.at must be rejected; got: {msg}"
    );
}

// ── C4 regression: unparseable DSL action rejected ───────────────────────────

#[tokio::test]
async fn c4_schedule_bogus_action_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "bogus-not-a-valid-verb()",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("DSL") || msg.contains("action") || msg.contains("invalid"),
        "C4: bogus action must be rejected with DSL error hint; got: {msg}"
    );
}

#[tokio::test]
async fn c4_schedule_single_char_action_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "x",
                "at": "2099-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("DSL") || msg.contains("action") || msg.contains("invalid"),
        "C4: single-char action must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn c4_schedule_valid_action_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"hello world\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule with valid DSL action must succeed");

    assert_eq!(result["status"], "pending");
}

// ── H5 regression: trigger_at preserves caller's original RFC3339 string ─────

#[tokio::test]
async fn h5_schedule_at_with_offset_preserves_original_string() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-01-02T00:00:00+02:00";
    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"tz-test\")",
                "at": input_at
            }),
        )
        .await
        .expect("schedule with +02:00 offset must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: trigger_at in response must preserve caller's original RFC3339 string; got {trigger_at}"
    );
    assert!(
        trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "H5: stored trigger_at must be a valid RFC 3339 timestamp; got {trigger_at}"
    );
}

#[tokio::test]
async fn h5_remind_at_with_offset_preserves_original_string() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-06-15T09:00:00+05:30";
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "tz-remind",
                "at": input_at
            }),
        )
        .await
        .expect("remind with +05:30 offset must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: trigger_at in remind response must preserve caller's original RFC3339 string; got {trigger_at}"
    );
    assert!(
        trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "H5: stored trigger_at must be a valid RFC 3339 timestamp; got {trigger_at}"
    );
}

#[tokio::test]
async fn h5_utc_input_preserved_as_is() {
    let (registry, _rt) = build_registry();

    let input_at = "2099-03-10T12:00:00Z";
    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "utc-tz-test", "at": input_at }),
        )
        .await
        .expect("remind with Z suffix must succeed");

    let trigger_at = result["trigger_at"].as_str().expect("trigger_at present");
    assert_eq!(
        trigger_at, input_at,
        "H5: UTC input must be returned unchanged; got {trigger_at}"
    );
}

// ── Repeat validation ───────────────────────────────────────────────────────

#[tokio::test]
async fn remind_with_invalid_repeat_is_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "hello",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "not-valid-cron"
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("repeat") || err.to_string().contains("cron"));
}

#[tokio::test]
async fn sch_aud_002_malformed_five_field_cron_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "bad cron",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "foo bar baz qux zap"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("repeat") || msg.contains("cron") || msg.contains("invalid"),
        "SCH-AUD-002: malformed five-field cron must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_out_of_range_cron_minute_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "bad minute",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "99 * * * *"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("minute") || msg.contains("range") || msg.contains("99"),
        "SCH-AUD-002: out-of-range minute field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_002_valid_wildcard_cron_accepted() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "wildcard cron",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "* * * * *"
            }),
        )
        .await
        .expect("SCH-AUD-002: all-wildcard cron must be accepted");
    assert_eq!(result["status"], "pending");
}

#[tokio::test]
async fn sch_aud_002_valid_numeric_cron_accepted() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "monday morning",
                "at": "2099-06-01T09:00:00Z",
                "repeat": "0 9 * * 1"
            }),
        )
        .await
        .expect("SCH-AUD-002: valid numeric cron must be accepted");
    assert_eq!(result["status"], "pending");
}
