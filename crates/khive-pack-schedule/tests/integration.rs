//! Smoke tests for the schedule pack (ADR-040).

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[test]
fn schedule_pack_declares_scheduled_event_note_kind() {
    assert!(SchedulePack::NOTE_KINDS.contains(&"scheduled_event"));
}

#[test]
fn schedule_pack_declares_four_handlers() {
    assert_eq!(SchedulePack::HANDLERS.len(), 4);
    let names: Vec<&str> = SchedulePack::HANDLERS.iter().map(|h| h.name).collect();
    assert!(names.contains(&"remind"));
    assert!(names.contains(&"schedule"));
    assert!(names.contains(&"agenda"));
    assert!(names.contains(&"cancel"));
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
            "remind",
            serde_json::json!({
                "content": "check status",
                "at": "2026-06-01T09:00:00Z"
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
            "schedule",
            serde_json::json!({
                "action": "create(kind=entity, name=test)",
                "at": "2026-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule succeeds");

    assert!(result.get("id").is_some(), "schedule returns id: {result}");
    assert_eq!(result["event_type"], "schedule");
}

#[tokio::test]
async fn agenda_returns_pending_events() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "remind",
            serde_json::json!({ "content": "hello", "at": "2026-07-01T00:00:00Z" }),
        )
        .await
        .expect("remind succeeds");

    let agenda = registry
        .dispatch("agenda", serde_json::json!({ "limit": 10 }))
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
            "remind",
            serde_json::json!({
                "content": "hello",
                "at": "2026-06-01T09:00:00Z",
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
            "remind",
            serde_json::json!({ "content": "check status", "at": "2026-06-01T09:00:00Z" }),
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

#[tokio::test]
async fn test_cancel_accepts_short_id() {
    let (registry, _rt) = build_registry();

    let reminded = registry
        .dispatch(
            "remind",
            serde_json::json!({ "content": "cancel me by short id", "at": "2026-07-01T12:00:00Z" }),
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
        .dispatch("cancel", serde_json::json!({ "id": short }))
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
