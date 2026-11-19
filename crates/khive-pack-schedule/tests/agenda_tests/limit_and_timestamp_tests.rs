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
