#[tokio::test]
async fn agenda_returns_pending_events() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "hello", "at": "2099-07-01T00:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 10 }))
        .await
        .expect("agenda succeeds");

    let count = agenda["count"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "agenda should return at least 1 event: {agenda}"
    );
}

#[tokio::test]
async fn s_c1_agenda_only_shows_valid_events() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({ "content": "valid event", "at": "2099-01-01T10:00:00Z" }),
        )
        .await
        .expect("remind with valid at must succeed");

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 50 }))
        .await
        .expect("agenda must succeed");

    let events = agenda["events"].as_array().expect("events array");
    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at must be a string");
        assert!(
            trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
            "S-C1/M-1: agenda event trigger_at {trigger_at:?} must be a valid RFC 3339 timestamp"
        );
    }
}

#[tokio::test]
async fn c3_agenda_never_shows_past_pending_events() {
    let (registry, _rt) = build_registry();

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
