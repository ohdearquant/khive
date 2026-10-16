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
async fn remind_persists_the_creating_actor_for_delivery() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("lambda:reminder-owner".to_string()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

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

    let id = result["full_id"]
        .as_str()
        .expect("full_id present")
        .parse()
        .expect("full_id is a UUID");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize");
    let note = runtime
        .notes(&token)
        .expect("notes")
        .get_note(id)
        .await
        .expect("read reminder")
        .expect("reminder exists");

    assert_eq!(
        note.properties
            .as_ref()
            .and_then(|props| props["created_by_actor"].as_str()),
        Some("lambda:reminder-owner")
    );
    let provenance = runtime
        .events(&token)
        .expect("events")
        .query_events(
            khive_storage::EventFilter {
                verbs: vec![khive_pack_schedule::CREATOR_PROVENANCE_VERB.to_string()],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query provenance");
    let event = provenance
        .items
        .iter()
        .find(|event| event.target_id == Some(id))
        .expect("target-bound reminder provenance");
    assert_eq!(event.actor, "actor:lambda:reminder-owner");
    assert_eq!(
        event.payload["provenance"],
        serde_json::Value::String(khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1.to_string())
    );
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
async fn schedule_persists_the_creating_actor_for_replay() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("lambda:schedule-owner".to_string()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", name=\"future concept\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule succeeds");

    let id = result["full_id"]
        .as_str()
        .expect("full_id present")
        .parse()
        .expect("full_id is a UUID");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize");
    let note = runtime
        .notes(&token)
        .expect("notes")
        .get_note(id)
        .await
        .expect("read scheduled action")
        .expect("scheduled action exists");

    assert_eq!(
        note.properties
            .as_ref()
            .and_then(|props| props["created_by_actor"].as_str()),
        Some("lambda:schedule-owner")
    );
    let provenance = runtime
        .events(&token)
        .expect("events")
        .query_events(
            khive_storage::EventFilter {
                verbs: vec![khive_pack_schedule::CREATOR_PROVENANCE_VERB.to_string()],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query provenance");
    let event = provenance
        .items
        .iter()
        .find(|event| event.target_id == Some(id))
        .expect("target-bound schedule provenance");
    assert_eq!(event.actor, "actor:lambda:schedule-owner");
    assert_eq!(
        event.payload["event_type"],
        serde_json::Value::String("schedule".to_string())
    );
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
