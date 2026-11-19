// ── H1 regression: agenda from/to uses parsed timestamps ─────────────────────

#[tokio::test]
async fn h1_agenda_from_filter_uses_parsed_timestamps() {
    let (registry, _rt) = build_registry();

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

    let agenda = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "from": "2099-06-01T00:00:00Z", "limit": 50 }),
        )
        .await
        .expect("agenda with from filter succeeds");

    let events = agenda["events"].as_array().expect("events array");
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
async fn agenda_window_and_ties_use_exact_utc_instants() {
    let (registry, _rt) = build_registry();
    for (content, at) in [
        ("next-day-tie", "2099-01-02T00:00:00+14:00"),
        ("utc-tie-b", "2099-01-01T10:00:00Z"),
        ("offset-tie", "2099-01-01T05:00:00-05:00"),
        ("before", "2099-01-01T15:59:59+06:00"),
        ("previous-day-tie", "2098-12-31T23:00:00-11:00"),
        ("utc-tie-a", "2099-01-01T10:00:00Z"),
        ("after", "2099-01-01T05:00:01-05:00"),
    ] {
        registry
            .dispatch(
                "schedule.remind",
                serde_json::json!({ "content": content, "at": at }),
            )
            .await
            .expect("reminder created");
    }
    let window = serde_json::json!({
        "from": "2099-01-01T10:00:00Z",
        "to": "2099-01-01T10:00:00Z",
        "limit": 10,
    });
    let agenda = registry
        .dispatch("schedule.agenda", window.clone())
        .await
        .expect("agenda succeeds");
    let events = agenda["events"].as_array().expect("events array");
    assert_eq!(events.len(), 5, "SQL must return only the exact UTC window");
    assert_eq!(events[0]["content"], "previous-day-tie");
    assert_eq!(events[1]["content"], "offset-tie");
    assert_eq!(events[4]["content"], "next-day-tie");
    let tied_ids: Vec<_> = events[2..4]
        .iter()
        .map(|event| event["full_id"].as_str().unwrap())
        .collect();
    assert!(tied_ids[0] < tied_ids[1], "equal text sorts by note ID");

    let limited = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({
                "from": window["from"], "to": window["to"], "limit": 2,
            }),
        )
        .await
        .expect("limited agenda succeeds");
    let limited_events = limited["events"].as_array().expect("events array");
    assert_eq!(limited_events.len(), 2);
    assert_eq!(
        limited_events.as_slice(),
        &events[..2],
        "a limit cutting an equal-instant group keeps the first raw-text/ID ties"
    );
    assert_eq!(limited_events[0]["content"], "previous-day-tie");
    assert_eq!(limited_events[1]["content"], "offset-tie");

    let unbounded = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 1 }))
        .await
        .expect("unbounded agenda succeeds");
    assert_eq!(unbounded["events"][0]["content"], "before");
    let from_only = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "from": "2099-01-01T10:00:00Z", "limit": 10 }),
        )
        .await
        .expect("from-only agenda succeeds");
    assert_eq!(from_only["count"], 6);
    let to_only = registry
        .dispatch(
            "schedule.agenda",
            serde_json::json!({ "to": "2099-01-01T10:00:00Z", "limit": 10 }),
        )
        .await
        .expect("to-only agenda succeeds");
    assert_eq!(to_only["count"], 6);
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
