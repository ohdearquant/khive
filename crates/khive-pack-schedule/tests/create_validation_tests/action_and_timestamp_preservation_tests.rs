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
                "action": "schedule.remind(content=\"hello world\", at=\"2099-12-31T00:00:00Z\")",
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
                "action": "schedule.remind(content=\"tz-test\", at=\"2099-12-31T00:00:00Z\")",
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
