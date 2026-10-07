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

#[tokio::test]
async fn agenda_windows_preserve_every_accepted_timestamp_spelling() {
    use khive_runtime::RuntimeConfig;
    use serde_json::json;

    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        disk_guard_config: None,
        volume_lock_dir: None,
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        events_split: None,
        blob: Default::default(),
        packs: vec!["kg".into(), "comm".into(), "schedule".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("isolated in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let store = runtime.notes(&token).unwrap();
    let mut seeded = Vec::new();
    let mut snapshots = Vec::new();
    for (group, at) in [
        (0u8, "2099-06-01T09:59:59Z"),
        (1, "2099-06-01T10:00:00Z"),
        (1, "2099-6-01T10:00:00Z"),
        (1, "2099-06-1T10:00:00Z"),
        (1, "2099- 06-01T10:00:00Z"),
        (1, "+2099-06-01T10:00:00Z"),
        (2, "2099-06-01T10:00:01Z"),
        (3, "+12099-06-01T10:00:00Z"),
    ] {
        for verb in ["schedule.remind", "schedule.schedule"] {
            let params = if verb == "schedule.remind" {
                json!({"content": "timestamp spelling", "at": at})
            } else {
                json!({"action": "stats()", "at": at})
            };
            let created = registry
                .dispatch(verb, params)
                .await
                .expect("accepted timestamp");
            assert_eq!(created["trigger_at"], at, "creation preserves the spelling");
            let id = created["full_id"].as_str().expect("full ID").to_string();
            let uuid = id.parse().expect("UUID");
            let note = store.get_note(uuid).await.unwrap().unwrap();
            snapshots.push((uuid, serde_json::to_value(note).unwrap()));
            seeded.push((group, at.to_string(), id));
        }
    }
    // Groups are fixture-declared UTC positions; ties use original text then UUID.
    seeded.sort();
    let bound = "2099-06-01T10:00:00Z";
    for (from, to, first_group, last_group) in [
        (None, None, 0, 3),
        (Some(bound), None, 1, 3),
        (None, Some(bound), 0, 1),
        (Some(bound), Some(bound), 1, 1),
    ] {
        let expected: Vec<String> = seeded
            .iter()
            .filter(|entry| entry.0 >= first_group && entry.0 <= last_group)
            .map(|entry| entry.2.clone())
            .collect();
        for limit in [3, 200] {
            let mut params = json!({"from": from, "to": to, "limit": limit});
            let mut seen = Vec::new();
            let mut completed = false;
            for _ in 0..=expected.len() {
                let page = registry
                    .dispatch("schedule.agenda", params.clone())
                    .await
                    .expect("agenda page");
                let events = page["events"].as_array().expect("events");
                assert_eq!(page["count"].as_u64(), Some(events.len() as u64));
                assert!(events.len() <= limit as usize);
                if events.is_empty() {
                    assert!(page["next"].is_null());
                    completed = true;
                    break;
                }
                for event in events {
                    let id = event["full_id"].as_str().expect("event ID");
                    let seed = seeded.iter().find(|entry| entry.2 == id).unwrap();
                    assert_eq!(
                        event["properties"]["trigger_at"].as_str(),
                        Some(seed.1.as_str())
                    );
                    seen.push(id.to_string());
                }
                let last = events.last().unwrap();
                assert_eq!(page["next"]["after"], last["properties"]["trigger_at"]);
                assert_eq!(page["next"]["after_id"], last["full_id"]);
                params["after"] = page["next"]["after"].clone();
                params["after_id"] = page["next"]["after_id"].clone();
            }
            assert!(completed, "continuation must reach an empty page");
            assert_eq!(seen, expected, "from={from:?}, to={to:?}, limit={limit}");
        }
    }
    for (id, before) in snapshots {
        let after = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            before,
            "agenda is read-only"
        );
    }
}
