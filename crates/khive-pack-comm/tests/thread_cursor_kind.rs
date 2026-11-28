use khive_pack_comm::CommPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistryBuilder};
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

fn row(id: &str, kind: &str, created_at: i64, root: Uuid) -> Note {
    Note {
        id: id.parse().unwrap(),
        namespace: "local".into(),
        kind: kind.into(),
        status: "active".into(),
        name: None,
        content: format!("cursor fixture {kind}"),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({
            "direction": "inbound", "from_actor": "sender",
            "to_actor": "cursor-reader", "read": false, "thread_id": root,
        })),
        created_at,
        updated_at: created_at,
        deleted_at: None,
        key: None,
        version: 1,
    }
}

fn ids(response: &Value) -> Vec<Uuid> {
    response["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["full_id"].as_str().unwrap().parse().unwrap())
        .collect()
}

#[tokio::test]
async fn thread_after_requires_message_kind_and_keeps_cursor_ordering() {
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.wal_ceiling_bytes = 0;
    config.wal_ceiling_configured_bytes = 0;
    config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
    config.wal_ceiling_env_raw = None;
    config.disk_guard_environment = Default::default();
    config.disk_guard_config = None;
    config.volume_lock_dir = None;
    config.credentials.clear();
    config.visibility_receipts = None;
    config.mounts.clear();
    config.events_split = None;
    config.actor_id = Some("cursor-reader".into());
    config.brain_profile = None;
    config.brain = Default::default();
    config.packs = vec!["kg".into(), "comm".into()];
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).unwrap();
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("cursor-reader".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.notes(&token).unwrap();
    let root: Uuid = "11111111-0000-4000-8000-000000000001".parse().unwrap();
    let other_root: Uuid = "55555555-0000-4000-8000-000000000005".parse().unwrap();
    let notes = [
        row(&root.to_string(), "message", 1_000_000, root),
        row(
            "22222222-0000-4000-8000-000000000002",
            "message",
            2_000_000,
            root,
        ),
        row(
            "33333333-0000-4000-8000-000000000003",
            "observation",
            3_000_000,
            root,
        ),
        row(
            "44444444-0000-4000-8000-000000000004",
            "message",
            4_000_000,
            root,
        ),
        row(&other_root.to_string(), "message", 3_000_000, other_root),
    ];
    for note in &notes {
        store.upsert_note(note.clone()).await.unwrap();
    }
    let baseline = registry
        .dispatch("comm.thread", json!({"id":root}))
        .await
        .unwrap();
    assert_eq!(ids(&baseline), vec![root, notes[1].id, notes[3].id]);
    for cursor in [notes[2].id.to_string(), "33333333".into()] {
        for order in ["asc", "desc"] {
            let result = registry
                .dispatch(
                    "comm.thread",
                    json!({"id":root,"after":cursor,"order":order}),
                )
                .await;
            assert!(
                matches!(&result, Err(RuntimeError::InvalidInput(_))),
                "{result:?}"
            );
            assert_eq!(result.unwrap_err().to_string(), format!("invalid input: thread: `after` cursor {cursor:?} does not resolve to a message"));
        }
    }
    for cursor in [
        notes[1].id.to_string(),
        "22222222".into(),
        "1970-01-01T00:00:02Z".into(),
    ] {
        let asc = registry
            .dispatch("comm.thread", json!({"id":root,"after":cursor}))
            .await
            .unwrap();
        assert_eq!(ids(&asc), vec![notes[3].id]);
        let desc = registry
            .dispatch(
                "comm.thread",
                json!({"id":root,"after":cursor,"order":"desc"}),
            )
            .await
            .unwrap();
        assert_eq!(ids(&desc), vec![root]);
    }
    let outside_thread = registry
        .dispatch("comm.thread", json!({"id":root,"after":other_root}))
        .await
        .unwrap();
    assert_eq!(ids(&outside_thread), vec![notes[3].id]);
    for note in &notes {
        assert_eq!(store.get_note(note.id).await.unwrap().as_ref(), Some(note));
    }
}

#[tokio::test]
async fn thread_hides_unaddressed_roots_and_cursors_without_hiding_visible_children() {
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.wal_ceiling_bytes = 0;
    config.wal_ceiling_configured_bytes = 0;
    config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
    config.wal_ceiling_env_raw = None;
    config.disk_guard_environment = Default::default();
    config.disk_guard_config = None;
    config.volume_lock_dir = None;
    config.credentials.clear();
    config.visibility_receipts = None;
    config.mounts.clear();
    config.events_split = None;
    config.actor_id = Some("cursor-reader".into());
    config.brain_profile = None;
    config.brain = Default::default();
    config.packs = vec!["kg".into(), "comm".into()];
    let runtime = KhiveRuntime::new(config).unwrap();
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("cursor-reader".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.notes(&token).unwrap();

    let root: Uuid = "11111111-0000-4000-8000-000000000001".parse().unwrap();
    let hidden_root: Uuid = "77777777-0000-4000-8000-000000000007".parse().unwrap();
    let missing: Uuid = "aaaaaaaa-0000-4000-8000-00000000000a".parse().unwrap();
    let visible_root = row(&root.to_string(), "message", 1_000_000, root);
    let visible_child = row(
        "22222222-0000-4000-8000-000000000002",
        "message",
        2_000_000,
        root,
    );
    let later = row(
        "44444444-0000-4000-8000-000000000004",
        "message",
        4_000_000,
        root,
    );
    let outside_id: Uuid = "55555555-0000-4000-8000-000000000005".parse().unwrap();
    let outside = row(&outside_id.to_string(), "message", 3_000_000, outside_id);
    let mut hidden = row(&hidden_root.to_string(), "message", 3_000_000, hidden_root);
    hidden.properties.as_mut().unwrap()["from_actor"] = json!("hidden-sender");
    hidden.properties.as_mut().unwrap()["to_actor"] = json!("hidden-recipient");
    let child_of_hidden = row(
        "88888888-0000-4000-8000-000000000008",
        "message",
        5_000_000,
        hidden_root,
    );
    let mut outbound = row(
        "bbbbbbbb-0000-4000-8000-00000000000b",
        "message",
        5_000_000,
        root,
    );
    outbound.properties.as_mut().unwrap()["direction"] = json!("outbound");
    outbound.properties.as_mut().unwrap()["from_actor"] = json!("cursor-reader");
    outbound.properties.as_mut().unwrap()["to_actor"] = json!("sender");
    let mut inbound = row(
        "cccccccc-0000-4000-8000-00000000000c",
        "message",
        6_000_000,
        root,
    );
    inbound.properties.as_mut().unwrap()["outbound_ref"] = json!(outbound.id.to_string());
    for note in [
        &visible_root,
        &visible_child,
        &later,
        &outside,
        &child_of_hidden,
        &outbound,
        &inbound,
    ] {
        store.upsert_note(note.clone()).await.unwrap();
    }
    let cursor_inputs = [hidden_root.to_string(), "77777777".to_string()];
    let mut unknown_cursors = Vec::new();
    for order in ["asc", "desc"] {
        for cursor in &cursor_inputs {
            let error = registry
                .dispatch(
                    "comm.thread",
                    json!({"id": root, "after": cursor, "order": order}),
                )
                .await
                .unwrap_err();
            assert!(matches!(&error, RuntimeError::InvalidInput(_)));
            assert_eq!(
                error.to_string(),
                format!("invalid input: thread: `after` cursor {cursor:?} does not resolve to a message")
            );
            unknown_cursors.push(error.to_string());
        }
    }
    store.upsert_note(hidden.clone()).await.unwrap();

    let baseline = registry
        .dispatch("comm.thread", json!({"id": root}))
        .await
        .unwrap();
    assert_eq!(
        ids(&baseline),
        vec![visible_root.id, visible_child.id, later.id, outbound.id]
    );
    let visible_prefix = registry
        .dispatch("comm.thread", json!({"id": "11111111"}))
        .await
        .unwrap();
    assert_eq!(ids(&visible_prefix), ids(&baseline));
    let anchor = registry
        .dispatch("comm.thread", json!({"id": root, "after": "22222222"}))
        .await
        .unwrap();
    assert_eq!(ids(&anchor), vec![later.id, outbound.id]);
    let outside_anchor = registry
        .dispatch("comm.thread", json!({"id": root, "after": outside.id}))
        .await
        .unwrap();
    assert_eq!(ids(&outside_anchor), vec![later.id, outbound.id]);
    let twin_anchor = registry
        .dispatch("comm.thread", json!({"id": root, "after": inbound.id}))
        .await
        .unwrap();
    assert!(ids(&twin_anchor).is_empty());
    let twin_entry = registry
        .dispatch("comm.thread", json!({"id": inbound.id}))
        .await
        .unwrap();
    assert_eq!(ids(&twin_entry), ids(&baseline));

    let visible_reply = registry
        .dispatch("comm.thread", json!({"id": child_of_hidden.id}))
        .await
        .unwrap();
    assert_eq!(visible_reply["thread_id"], hidden_root.to_string());
    assert_eq!(ids(&visible_reply), vec![child_of_hidden.id]);

    let missing_root = registry
        .dispatch("comm.thread", json!({"id": missing}))
        .await
        .unwrap_err()
        .to_string();
    for hidden_id in [hidden_root.to_string(), "77777777".into()] {
        let error = registry
            .dispatch("comm.thread", json!({"id": hidden_id}))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, missing_root);
    }
    let missing_prefix = registry
        .dispatch("comm.thread", json!({"id": "aaaaaaaa"}))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(missing_prefix, missing_root);

    for (order, unknowns) in ["asc", "desc"].into_iter().zip(unknown_cursors.chunks(2)) {
        for (hidden_id, unknown) in cursor_inputs.iter().zip(unknowns) {
            let error = registry
                .dispatch(
                    "comm.thread",
                    json!({"id": root, "after": hidden_id, "order": order}),
                )
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(&error, unknown);
        }
    }
}

#[tokio::test]
async fn anonymous_local_alone_can_use_legacy_roots_and_cursors() {
    fn config(actor: Option<&str>) -> RuntimeConfig {
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = None;
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
        config.wal_ceiling_env_raw = None;
        config.disk_guard_environment = Default::default();
        config.disk_guard_config = None;
        config.volume_lock_dir = None;
        config.credentials.clear();
        config.visibility_receipts = None;
        config.mounts.clear();
        config.events_split = None;
        config.actor_id = actor.map(str::to_owned);
        config.brain_profile = None;
        config.brain = Default::default();
        config.packs = vec!["kg".into(), "comm".into()];
        config
    }

    let backend = khive_db::StorageBackend::memory().unwrap();
    backend.pool().run_migrations().unwrap();
    let backend = std::sync::Arc::new(backend);
    let anonymous = KhiveRuntime::from_backend(backend.clone(), config(None));
    let named = KhiveRuntime::from_backend(backend, config(Some("cursor-reader")));
    let token = anonymous.authorize(Namespace::local()).unwrap();
    assert!(token.actor().is_anonymous());
    assert_eq!(token.actor().id, "local");

    let mut anonymous_builder = VerbRegistryBuilder::new();
    anonymous_builder.with_actor_id(None);
    anonymous_builder.register(khive_pack_kg::KgPack::new(anonymous.clone()));
    anonymous_builder.register(CommPack::new(anonymous.clone()));
    let anonymous_registry = anonymous_builder.build().unwrap();
    let whoami = anonymous_registry
        .dispatch("whoami", json!({}))
        .await
        .unwrap();
    assert_eq!(whoami["actor_id"], "local");
    assert_eq!(whoami["unattributed"], true);

    let mut named_builder = VerbRegistryBuilder::new();
    named_builder.with_actor_id(Some("cursor-reader".into()));
    named_builder.register(khive_pack_kg::KgPack::new(named.clone()));
    named_builder.register(CommPack::new(named));
    let named_registry = named_builder.build().unwrap();

    let root: Uuid = "11111111-0000-4000-8000-000000000001".parse().unwrap();
    let missing: Uuid = "aaaaaaaa-0000-4000-8000-00000000000a".parse().unwrap();
    let mut legacy_root = row(&root.to_string(), "message", 1_000_000, root);
    legacy_root
        .properties
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("to_actor");
    let mut legacy_cursor = row(
        "22222222-0000-4000-8000-000000000002",
        "message",
        2_000_000,
        root,
    );
    legacy_cursor.properties.as_mut().unwrap()["to_actor"] = Value::Null;
    let mut legacy_later = row(
        "33333333-0000-4000-8000-000000000003",
        "message",
        3_000_000,
        root,
    );
    legacy_later
        .properties
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("to_actor");
    let object_id: Uuid = "44444444-0000-4000-8000-000000000004".parse().unwrap();
    let mut object_recipient = row(&object_id.to_string(), "message", 4_000_000, object_id);
    object_recipient.properties.as_mut().unwrap()["to_actor"] = json!({});
    let addressed_id: Uuid = "55555555-0000-4000-8000-000000000005".parse().unwrap();
    let addressed = row(
        &addressed_id.to_string(),
        "message",
        5_000_000,
        addressed_id,
    );
    let store = anonymous.notes(&token).unwrap();
    for note in [&legacy_root, &legacy_later, &addressed] {
        store.upsert_note(note.clone()).await.unwrap();
    }
    let legacy_inputs = [legacy_cursor.id.to_string(), "22222222".to_string()];
    let object_inputs = [object_id.to_string(), "44444444".to_string()];
    let mut named_unknown_cursors = Vec::new();
    let mut anonymous_unknown_cursors = Vec::new();
    for order in ["asc", "desc"] {
        for cursor in &legacy_inputs {
            let error = named_registry
                .dispatch(
                    "comm.thread",
                    json!({"id": addressed_id, "after": cursor, "order": order}),
                )
                .await
                .unwrap_err();
            assert!(matches!(&error, RuntimeError::InvalidInput(_)));
            named_unknown_cursors.push(error.to_string());
        }
        for cursor in &object_inputs {
            let error = anonymous_registry
                .dispatch(
                    "comm.thread",
                    json!({"id": root, "after": cursor, "order": order}),
                )
                .await
                .unwrap_err();
            assert!(matches!(&error, RuntimeError::InvalidInput(_)));
            anonymous_unknown_cursors.push(error.to_string());
        }
    }
    store.upsert_note(legacy_cursor.clone()).await.unwrap();
    store.upsert_note(object_recipient.clone()).await.unwrap();

    let baseline = anonymous_registry
        .dispatch("comm.thread", json!({"id": root}))
        .await
        .unwrap();
    assert_eq!(
        ids(&baseline),
        vec![root, legacy_cursor.id, legacy_later.id]
    );
    let after_legacy = anonymous_registry
        .dispatch(
            "comm.thread",
            json!({"id": root, "after": legacy_cursor.id}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&after_legacy), vec![legacy_later.id]);
    let named_baseline = named_registry
        .dispatch("comm.thread", json!({"id": addressed_id}))
        .await
        .unwrap();
    assert_eq!(ids(&named_baseline), vec![addressed_id]);

    let missing_root = named_registry
        .dispatch("comm.thread", json!({"id": missing}))
        .await
        .unwrap_err()
        .to_string();
    let hidden_root = named_registry
        .dispatch("comm.thread", json!({"id": root}))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(hidden_root, missing_root);
    for (order, unknowns) in ["asc", "desc"]
        .into_iter()
        .zip(named_unknown_cursors.chunks(2))
    {
        for (cursor, unknown) in legacy_inputs.iter().zip(unknowns) {
            let error = named_registry
                .dispatch(
                    "comm.thread",
                    json!({"id": addressed_id, "after": cursor, "order": order}),
                )
                .await
                .unwrap_err();
            assert_eq!(&error.to_string(), unknown);
        }
    }

    let anonymous_missing = anonymous_registry
        .dispatch("comm.thread", json!({"id": missing}))
        .await
        .unwrap_err()
        .to_string();
    let object_root = anonymous_registry
        .dispatch("comm.thread", json!({"id": object_id}))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(object_root, anonymous_missing);
    for (order, unknowns) in ["asc", "desc"]
        .into_iter()
        .zip(anonymous_unknown_cursors.chunks(2))
    {
        for (cursor, unknown) in object_inputs.iter().zip(unknowns) {
            let error = anonymous_registry
                .dispatch(
                    "comm.thread",
                    json!({"id": root, "after": cursor, "order": order}),
                )
                .await
                .unwrap_err();
            assert_eq!(&error.to_string(), unknown);
        }
    }
}
