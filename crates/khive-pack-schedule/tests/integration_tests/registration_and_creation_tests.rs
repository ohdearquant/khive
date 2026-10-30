#[test]
fn schedule_pack_declares_scheduled_event_note_kind() {
    assert!(SchedulePack::NOTE_KINDS.contains(&"scheduled_event"));
}

#[test]
fn schedule_pack_declares_six_handlers() {
    assert_eq!(SchedulePack::HANDLERS.len(), 6);
    let names: Vec<&str> = SchedulePack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"schedule.remind"));
    assert!(names.contains(&"schedule.schedule"));
    assert!(names.contains(&"schedule.agenda"));
    assert!(names.contains(&"schedule.cancel"));
    assert!(names.contains(&"schedule.occurrences"));
    assert!(names.contains(&"schedule.receipts"));
}

#[test]
fn schedule_pack_requires_kg() {
    assert_eq!(SchedulePack::REQUIRES, &["kg"]);
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
                "action": "create(kind=\"concept\", name=\"test\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule succeeds");

    assert!(result.get("id").is_some(), "schedule returns id: {result}");
    assert_eq!(result["event_type"], "schedule");
}

#[tokio::test]
async fn monthly_creation_stores_original_trigger_as_repeat_anchor_for_both_verbs() {
    let (registry, runtime) = build_registry();
    let at = "2099-01-31T09:30:00+04:00";
    for (verb, params) in [
        (
            "schedule.remind",
            serde_json::json!({"content": "month-end reminder", "at": at, "repeat": "monthly"}),
        ),
        (
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", name=\"month-end action\")",
                "at": at,
                "repeat": "monthly"
            }),
        ),
    ] {
        let created = registry
            .dispatch(verb, params)
            .await
            .expect("create monthly event");
        let props = event_properties(&runtime, created["full_id"].as_str().expect("full_id")).await;
        assert_eq!(props["trigger_at"], at, "{verb}: original trigger");
        assert_eq!(props["repeat_anchor"], at, "{verb}: immutable anchor");
    }
}

#[tokio::test]
async fn nonmonthly_creation_does_not_store_repeat_anchor() {
    let (registry, runtime) = build_registry();
    for repeat in ["daily", "weekly", "every:15m", "0 9 * * 1"] {
        let created = registry
            .dispatch(
                "schedule.remind",
                serde_json::json!({
                    "content": "nonmonthly reminder",
                    "at": "2099-06-01T09:30:00Z",
                    "repeat": repeat,
                }),
            )
            .await
            .expect("create nonmonthly reminder");
        let props = event_properties(&runtime, created["full_id"].as_str().expect("full_id")).await;
        assert!(
            props.get("repeat_anchor").is_none(),
            "{repeat} must not store a monthly anchor"
        );
    }
}

#[tokio::test]
async fn generic_update_cannot_rewrite_monthly_repeat_anchor() {
    let (registry, runtime) = build_registry();
    let at = "2099-01-31T09:30:00Z";
    let created = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({"content": "protected anchor", "at": at, "repeat": "monthly"}),
        )
        .await
        .expect("create monthly reminder");
    let full_id = created["full_id"].as_str().expect("full_id");
    let error = registry
        .dispatch(
            "update",
            serde_json::json!({
                "id": full_id,
                "properties": {"repeat_anchor": "2099-01-01T09:30:00Z"}
            }),
        )
        .await
        .expect_err("generic update must refuse a schedule-managed note");
    assert!(
        error
            .to_string()
            .contains("scheduled_event notes are not editable via `update`"),
        "unexpected refusal: {error}"
    );
    let props = event_properties(&runtime, full_id).await;
    assert_eq!(props["repeat_anchor"], at, "refusal must preserve anchor");
    assert_eq!(props["trigger_at"], at, "refusal must preserve trigger");
}

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
