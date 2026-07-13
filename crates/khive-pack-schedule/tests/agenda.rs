//! Agenda query, filtering, pagination, and limit validation tests.

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

// ── Limit validation ────────────────────────────────────────────────────────

#[tokio::test]
async fn sch_aud_003_agenda_limit_zero_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 0 }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("range") || msg.contains('0'),
        "SCH-AUD-003: limit=0 must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_003_agenda_limit_over_max_rejected() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("schedule.agenda", serde_json::json!({ "limit": 201 }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("limit") || msg.contains("range") || msg.contains("201"),
        "SCH-AUD-003: limit=201 must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn sch_aud_003_agenda_limit_boundary_values_accepted() {
    let (registry, _rt) = build_registry();

    for limit in [1u32, 200u32] {
        registry
            .dispatch("schedule.agenda", serde_json::json!({ "limit": limit }))
            .await
            .unwrap_or_else(|e| panic!("SCH-AUD-003: limit={limit} must be accepted; got: {e}"));
    }
}
