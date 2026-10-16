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
