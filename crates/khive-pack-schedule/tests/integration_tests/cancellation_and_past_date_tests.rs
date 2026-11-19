#[tokio::test]
async fn test_cancel_accepts_short_id() {
    let (registry, _rt) = build_registry();

    let reminded = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "cancel me by short id", "at": "2099-07-01T12:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let short = reminded
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id present");
    let full_id = reminded
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("full_id present");
    assert_eq!(full_id.len(), 36, "full_id from remind must be 36-char");

    // Cancel using only the 8-char short prefix — must succeed.
    let result = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": short }))
        .await
        .expect("cancel with 8-char short id succeeds");

    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("cancelled"),
        "cancel returns status=cancelled — got {result}"
    );
    let cancel_full_id = result
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("cancel returns full_id");
    assert_eq!(
        cancel_full_id.len(),
        36,
        "cancel response full_id must be 36-char"
    );
    assert!(
        cancel_full_id.starts_with(short),
        "cancel response full_id starts with short prefix"
    );
}

// ── Regression: cancel already-cancelled event ──────────────────────────────

#[tokio::test]
async fn cancel_rejects_already_cancelled_event() {
    let (registry, _rt) = build_registry();

    let reminded = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "cancel once",
                "at": "2099-07-01T12:00:00Z"
            }),
        )
        .await
        .expect("remind succeeds");
    let full_id = reminded["full_id"].as_str().expect("full_id present");

    let first = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": full_id }))
        .await
        .expect("first cancel succeeds");
    assert_eq!(
        first["status"].as_str(),
        Some("cancelled"),
        "first cancel must return status=cancelled"
    );

    let err = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": full_id }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not pending") && msg.contains("cancelled"),
        "second cancel must report the event is no longer pending, got: {msg}"
    );
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

#[tokio::test]
async fn c3_agenda_never_shows_past_pending_events() {
    // Since past dates are rejected at write time, agenda must never contain
    // pending events with trigger_at in the past.
    let (registry, _rt) = build_registry();

    // Insert one valid future event.
    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "future check", "at": "2099-12-31T23:59:59Z" }),
        )
        .await
        .expect("future remind must succeed");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 100 }))
        .await
        .expect("agenda must succeed");

    let now = chrono::Utc::now();
    let events = agenda["events"].as_array().expect("events array");
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at must be present");
        let instant = trigger_at
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("trigger_at must be parseable");
        assert!(
            instant > now,
            "C3: agenda must never contain past-pending events; found {trigger_at}"
        );
    }
}
