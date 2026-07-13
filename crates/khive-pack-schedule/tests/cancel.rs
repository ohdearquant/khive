//! Cancel verb tests: short-id resolution, idempotency, corrupt properties.

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

mod support;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
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
    let msg = err.to_string();
    assert!(
        msg.contains("not pending") && msg.contains("cancelled"),
        "second cancel must report the event is no longer pending, got: {msg}"
    );
}

/// Issue #462: cancel must reject a `fired` event (not just `cancelled`) and
/// must never clobber `fired_at` via a full-row overwrite.
#[tokio::test]
async fn cancel_rejects_fired_event_without_clobbering_fired_at() {
    use khive_storage::Note;

    let runtime = support::memory_runtime();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");

    let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note_store = runtime.notes(&tok).expect("note store accessible");

    let fired_at = "2026-01-01T00:00:00Z".to_string();
    let event_id = uuid::Uuid::new_v4();
    let fired = Note {
        id: event_id,
        namespace: "local".to_string(),
        kind: "scheduled_event".to_string(),
        status: "active".to_string(),
        name: None,
        content: "already fired".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(serde_json::json!({
            "trigger_at": "2020-01-01T00:00:00Z",
            "repeat": null,
            "status": "fired",
            "event_type": "remind",
            "payload": null,
            "fired_at": fired_at,
            "cancelled_at": null,
        })),
        created_at: chrono::Utc::now().timestamp_micros(),
        updated_at: chrono::Utc::now().timestamp_micros(),
        deleted_at: None,
    };
    note_store
        .upsert_note(fired)
        .await
        .expect("fired note inserted");

    let err = registry
        .dispatch(
            "schedule.cancel",
            serde_json::json!({ "id": event_id.to_string() }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not pending") || msg.contains("fired"),
        "#462: cancel of a fired event must be rejected; got: {msg}"
    );

    let reloaded = note_store
        .get_note(event_id)
        .await
        .expect("get_note ok")
        .expect("note still present");
    let props = reloaded.properties.expect("properties present");
    assert_eq!(
        props.get("status").and_then(|v| v.as_str()),
        Some("fired"),
        "#462: rejected cancel must not change status away from fired"
    );
    assert_eq!(
        props.get("fired_at").and_then(|v| v.as_str()),
        Some(fired_at.as_str()),
        "#462: rejected cancel must not clobber fired_at"
    );
    assert!(
        props
            .get("cancelled_at")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "#462: rejected cancel must not set cancelled_at"
    );
}

/// Issue #462: any non-pending status (not just "cancelled" or "fired") is
/// rejected -- cancel is a strict pending -> cancelled transition.
#[tokio::test]
async fn cancel_rejects_non_pending_statuses() {
    use khive_storage::Note;

    for status in ["fired", "cancelled", "bogus-status"] {
        let runtime = support::memory_runtime();
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
        builder.register(SchedulePack::new(runtime.clone()));
        let registry = builder.build().expect("registry builds");

        let tok = runtime.authorize(khive_types::Namespace::local()).unwrap();
        let note_store = runtime.notes(&tok).expect("note store accessible");

        let event_id = uuid::Uuid::new_v4();
        let note = Note {
            id: event_id,
            namespace: "local".to_string(),
            kind: "scheduled_event".to_string(),
            status: "active".to_string(),
            name: None,
            content: format!("status={status}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(serde_json::json!({
                "trigger_at": "2020-01-01T00:00:00Z",
                "repeat": null,
                "status": status,
                "event_type": "remind",
                "payload": null,
                "fired_at": null,
                "cancelled_at": null,
            })),
            created_at: chrono::Utc::now().timestamp_micros(),
            updated_at: chrono::Utc::now().timestamp_micros(),
            deleted_at: None,
        };
        note_store.upsert_note(note).await.expect("note inserted");

        let err = registry
            .dispatch(
                "schedule.cancel",
                serde_json::json!({ "id": event_id.to_string() }),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not pending"),
            "#462: status {status:?} must not transition to cancelled; got: {err}"
        );

        let reloaded = note_store
            .get_note(event_id)
            .await
            .expect("get_note ok")
            .expect("note still present");
        assert_eq!(
            reloaded
                .properties
                .as_ref()
                .and_then(|p| p.get("status"))
                .and_then(|v| v.as_str()),
            Some(status),
            "#462: status {status:?} must be unchanged after rejected cancel"
        );
    }
}

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
