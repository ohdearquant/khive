// ── S-C1 regression: RFC 3339 timestamp validation ──────────────────────────

#[tokio::test]
async fn s_c1_schedule_valid_rfc3339_succeeds() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
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
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
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
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
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
                "action": "schedule.remind(content=\"test\", at=\"2099-12-31T00:00:00Z\")",
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
                "action": "schedule.remind(content=\"past\", at=\"2099-12-31T00:00:00Z\")",
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
