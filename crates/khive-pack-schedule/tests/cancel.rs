//! Cancel verb tests: short-id resolution, idempotency, corrupt properties.

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

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

    let result = registry
        .dispatch("schedule.cancel", serde_json::json!({ "id": short }))
        .await
        .expect("cancel with 8-char short id succeeds");

    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("cancelled"),
        "cancel returns status=cancelled -- got {result}"
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
    assert!(
        err.to_string().contains("already cancelled"),
        "second cancel must report already-cancelled state, got: {err:?}"
    );
}

#[tokio::test]
async fn sch_aud_001_cancel_with_string_properties_returns_error() {
    use khive_storage::Note;

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    let corrupt_id = uuid::Uuid::new_v4();
    let corrupt = Note {
        id: corrupt_id,
        namespace: "local".to_string(),
        kind: "scheduled_event".to_string(),
        status: "active".to_string(),
        name: None,
        content: "corrupt-props".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(serde_json::json!("not-an-object")),
        created_at: chrono::Utc::now().timestamp_micros(),
        updated_at: chrono::Utc::now().timestamp_micros(),
        deleted_at: None,
    };
    note_store
        .upsert_note(corrupt)
        .await
        .expect("corrupt note inserted");

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
