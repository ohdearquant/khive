// ── H2 regression: agenda paginates past corrupt legacy rows ─────────────────

#[tokio::test]
async fn h2_agenda_finds_valid_event_past_corrupt_legacy_rows() {
    use chrono::Utc;
    use khive_storage::Note;
    use serde_json::json;

    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    let valid_at = "2099-11-11T11:11:11Z";
    let valid_note = Note {
        version: 1,
        key: None,
        id: uuid::Uuid::new_v4(),
        namespace: "local".to_string(),
        kind: "scheduled_event".to_string(),
        status: "active".to_string(),
        name: None,
        content: "valid-event".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({
            "trigger_at": valid_at,
            "status": "pending",
            "event_type": "remind",
            "payload": null,
            "fired_at": null,
            "cancelled_at": null,
        })),
        created_at: 1_700_000_000_000_000_i64,
        updated_at: Utc::now().timestamp_micros(),
        deleted_at: None,
    };
    note_store
        .upsert_note(valid_note)
        .await
        .expect("valid note inserted");

    let now_micros = Utc::now().timestamp_micros();
    for i in 0..250u32 {
        let corrupt = Note {
            version: 1,
            key: None,
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "scheduled_event".to_string(),
            status: "active".to_string(),
            name: None,
            content: format!("corrupt-legacy-{i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({
                "trigger_at": "not-a-date",
                "status": "pending",
                "event_type": "remind",
                "payload": null,
                "fired_at": null,
                "cancelled_at": null,
            })),
            created_at: now_micros + (i as i64 * 1000),
            updated_at: now_micros,
            deleted_at: None,
        };
        note_store
            .upsert_note(corrupt)
            .await
            .expect("corrupt note inserted");
    }

    let agenda = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 10 }))
        .await
        .expect("agenda must succeed");

    let events = agenda["events"].as_array().expect("events array");
    assert!(
        !events.is_empty(),
        "H2: agenda must return at least one event; corrupt legacy rows must not hide valid ones"
    );

    for event in events {
        let trigger_at = event["properties"]["trigger_at"]
            .as_str()
            .expect("trigger_at present");
        assert!(
            trigger_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
            "H2: every agenda event must have a valid RFC 3339 trigger_at; got {trigger_at:?}"
        );
    }

    let found = events
        .iter()
        .any(|e| e["properties"]["trigger_at"].as_str() == Some(valid_at));
    assert!(
        found,
        "H2: valid-event with trigger_at={valid_at:?} must appear in agenda; got: {events:?}"
    );
}
