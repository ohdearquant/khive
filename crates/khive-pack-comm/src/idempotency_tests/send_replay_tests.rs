fn actor_registry(
    backend: Arc<khive_db::StorageBackend>,
    actor: &str,
    namespace: &str,
) -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            exec: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::parse(namespace).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some(actor.to_string()),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        },
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_actor_id(Some(actor.to_string()));
    builder.with_default_namespace(namespace);
    (builder.build().expect("actor registry"), runtime)
}

fn parties() -> (VerbRegistry, VerbRegistry, KhiveRuntime) {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    backend.pool().run_migrations().expect("migrations");
    let backend = Arc::new(backend);
    let (sender, runtime) = actor_registry(Arc::clone(&backend), "actor:sender", "local");
    let (recipient, _) = actor_registry(backend, "actor:recipient", "local");
    (sender, recipient, runtime)
}

fn file_parties() -> (tempfile::TempDir, VerbRegistry, VerbRegistry, KhiveRuntime) {
    let dir = tempfile::tempdir().unwrap();
    let backend =
        khive_db::StorageBackend::sqlite_for_test(dir.path().join("messages.db")).unwrap();
    backend.pool().run_migrations().unwrap();
    assert!(backend.pool().writer_task_handle().unwrap().is_some());
    let backend = Arc::new(backend);
    let (sender, runtime) = actor_registry(Arc::clone(&backend), "actor:sender", "local");
    let (recipient, _) = actor_registry(backend, "actor:recipient", "local");
    (dir, sender, recipient, runtime)
}

async fn message_count(runtime: &KhiveRuntime, keyed: bool) -> i64 {
    let sql = if keyed {
        "SELECT count(*) FROM notes WHERE kind = 'message' AND key IS NOT NULL AND deleted_at IS NULL"
    } else {
        "SELECT count(*) FROM notes WHERE kind = 'message' AND deleted_at IS NULL"
    };
    let result = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("idempotency-population".into()),
        })
        .await
        .unwrap();
    match result {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected count: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_send_replay_preserves_pair_population_and_both_ids() {
    let (sender, recipient, runtime) = parties();
    let args = json!({"to":"actor:recipient","subject":"subject","content":"same mail","idempotency_key":"operation-one"});
    let original = sender
        .dispatch("comm.send", args.clone())
        .await
        .expect("first keyed send");
    assert_eq!(message_count(&runtime, false).await, 2);
    let replay = sender
        .dispatch("comm.send", args)
        .await
        .expect("lost-ACK replay");
    assert_eq!(
        message_count(&runtime, false).await,
        2,
        "replay must not create a second pair"
    );
    assert_eq!(
        message_count(&runtime, true).await,
        1,
        "only outbound claims the key"
    );
    assert_eq!(original["replayed"], false);
    assert_eq!(replay["replayed"], true);
    for field in [
        "full_id",
        "recipient_id",
        "sent_at",
        "thread_id",
        "idempotency_key",
    ] {
        assert_eq!(
            replay[field], original[field],
            "stable receipt field {field}"
        );
        assert!(!replay[field].is_null(), "receipt field {field} must exist");
    }
    for (registry, box_name, expected_id) in [
        (&sender, "sent", &original["full_id"]),
        (&recipient, "inbox", &original["recipient_id"]),
    ] {
        let mut params = json!({"box": box_name});
        if box_name == "inbox" {
            params["status"] = json!("all");
        }
        let result = registry.dispatch("comm.inbox", params).await.unwrap();
        assert_eq!(result["count"], 1);
        let row = &result["messages"][0];
        assert_eq!(&row["full_id"], expected_id);
        assert_eq!(row["properties"]["idempotency_key"], "operation-one");
    }
}

fn args(key: &str) -> Value {
    json!({"to":"actor:recipient","subject":"subject","content":"same mail","idempotency_key":key})
}

async fn snapshot(runtime: &KhiveRuntime) -> String {
    let value = runtime.sql().reader().await.unwrap().query_scalar(SqlStatement {
        sql: "SELECT json_group_array(json_object('id',id,'content',content,'properties',properties,'key',key,'created_at',created_at,'updated_at',updated_at,'deleted_at',deleted_at)) FROM (SELECT * FROM notes WHERE kind='message' ORDER BY id)".into(),
        params: vec![], label: Some("idempotency-domain-snapshot".into()),
    }).await.unwrap();
    match value {
        Some(SqlValue::Text(value)) => value,
        other => panic!("unexpected domain snapshot: {other:?}"),
    }
}

fn conflict(error: RuntimeError, expected_id: &Value) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected structured key conflict: {error:?}")
    };
    assert_eq!(error.kind(), khive_types::ErrorKind::Conflict);
    let details = error.details().unwrap();
    assert_eq!(details.get("reason"), Some("key_conflict"));
    assert_eq!(details.get("existing_id"), expected_id.as_str());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_conflicts_preserve_all_message_rows() {
    let (sender, _, runtime) = parties();
    let original = sender.dispatch("comm.send", args("payload")).await.unwrap();
    let before = snapshot(&runtime).await;
    for (field, value) in [
        ("content", json!("different")),
        ("subject", Value::Null),
        ("to", json!("actor:else")),
        ("tags", json!(["different"])),
        ("thread_id", original["full_id"].clone()),
    ] {
        let mut changed = args("payload");
        changed[field] = value;
        conflict(
            sender.dispatch("comm.send", changed).await.unwrap_err(),
            &original["full_id"],
        );
        assert_eq!(
            snapshot(&runtime).await,
            before,
            "conflict mutated domain for {field}"
        );
    }
    let mut equivalent = args("payload");
    equivalent["to"] = json!(" actor:recipient ");
    equivalent["tags"] = json!([]);
    let replay = sender.dispatch("comm.send", equivalent).await.unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["full_id"], original["full_id"]);
    assert_eq!(snapshot(&runtime).await, before);
}
