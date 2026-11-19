// ── SCH-AUD-001 regression: cancel non-object properties returns error ────────

#[tokio::test]
async fn sch_aud_001_cancel_with_string_properties_returns_error() {
    use khive_storage::Note;

    let runtime = support::memory_runtime();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    // Insert a scheduled_event with string (non-object) properties — simulates
    // a corrupt row that would previously cause a panic in handle_cancel.
    let corrupt_id = uuid::Uuid::new_v4();
    let corrupt = Note {
        version: 1,
        key: None,
        id: corrupt_id,
        namespace: "local".to_string(),
        kind: "scheduled_event".to_string(),
        status: "active".to_string(),
        name: None,
        content: "corrupt-props".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        // Properties is a string instead of an object.
        properties: Some(serde_json::json!("not-an-object")),
        created_at: chrono::Utc::now().timestamp_micros(),
        updated_at: chrono::Utc::now().timestamp_micros(),
        deleted_at: None,
    };
    note_store
        .upsert_note(corrupt)
        .await
        .expect("corrupt note inserted");

    // cancel must return an error, NOT panic.
    let err = registry
        .dispatch(
            "schedule.cancel",
            serde_json::json!({ "id": corrupt_id.to_string() }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("malformed") || msg.contains("properties") || msg.contains("object"),
        "SCH-AUD-001: cancel with non-object properties must return an error, not panic; got: {msg}"
    );
}
