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

    // A well-formed verb call must be accepted.
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

// ── H1 regression: agenda from/to uses parsed timestamps ─────────────────────

#[tokio::test]
async fn h1_agenda_from_filter_uses_parsed_timestamps() {
    let (registry, _rt) = build_registry();

    // Insert events at two different future times.
    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "early", "at": "2099-01-01T10:00:00Z" }),
        )
        .await
        .expect("remind 1 succeeds");
    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "late", "at": "2099-12-31T10:00:00Z" }),
        )
        .await
        .expect("remind 2 succeeds");

    // Only events at or after 2099-06-01 should be returned.
    let agenda = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "from": "2099-06-01T00:00:00Z", "limit": 50 }),
        )
        .await
        .expect("agenda with from filter succeeds");

    let events = agenda["events"].as_array().expect("events array");
    // The early event (2099-01-01) must not appear.
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at present");
        let instant = trigger_at
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("parseable");
        let from = "2099-06-01T00:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        assert!(
            instant >= from,
            "H1: agenda.from filter must exclude events before the bound; found {trigger_at}"
        );
    }
}

#[tokio::test]
async fn h1_agenda_rejects_invalid_from() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "from": "not-a-date", "limit": 10 }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "H1: invalid agenda.from must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn h1_agenda_rejects_invalid_to() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "to": "not-a-date", "limit": 10 }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("RFC 3339") || msg.contains("timestamp") || msg.contains("not-a-date"),
        "H1: invalid agenda.to must be rejected; got: {msg}"
    );
}
