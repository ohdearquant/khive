#[tokio::test]
async fn cite_reports_requested_and_stored_weights() {
    let config = khive_runtime::RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        packs: vec!["kg".into(), "knowledge".into()],
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::new(config).expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("cite-weight-fixture".into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    let fixture = Fixture { registry };

    for (index, (supplied, requested, effective)) in [
        (Some(json!(-0.5)), json!(-0.5), 0.0),
        (Some(json!(1.5)), json!(1.5), 1.0),
        (Some(json!(0.4)), json!(0.4), 0.4),
        (None, Value::Null, 1.0),
        (Some(Value::Null), Value::Null, 1.0),
    ]
    .into_iter()
    .enumerate()
    {
        let concept = fixture
            .dispatch(
                "knowledge.learn",
                json!({
                    "name": format!("Cite weight concept {index}")
                }),
            )
            .await
            .expect("create concept");
        let source = fixture.dispatch("create", json!({
            "kind": "document",
            "name": format!("Cite weight source {index}"),
            "description": "A source document covering vector retrieval, graph provenance, semantic search, indexing, ranking, and reproducible citation metadata for research.",
            "skip_dedup_check": true
        })).await.expect("create document");
        let concept_id = concept["full_id"].as_str().unwrap();
        let source_id = source["id"].as_str().unwrap();
        let mut args = json!({"concept_id": concept_id, "source_id": source_id});
        if let Some(weight) = supplied {
            args["weight"] = weight;
        }
        let response = fixture
            .dispatch("knowledge.cite", args)
            .await
            .expect("cite");
        assert_eq!(
            response.get("weight_requested"),
            Some(&requested),
            "case {index}"
        );
        assert_eq!(response["weight"], json!(effective), "case {index}");
        assert_eq!(response["concept_id"], concept_id);
        assert_eq!(response["source_id"], source_id);
        assert_eq!(response["relation"], "introduced_by");
        let edge_id = response["full_id"].as_str().expect("full edge UUID");
        assert_eq!(uuid::Uuid::parse_str(edge_id).unwrap().to_string(), edge_id);
        let stored = fixture
            .dispatch("get", json!({"id": edge_id}))
            .await
            .expect("stored edge");
        assert_eq!(stored["kind"], "edge");
        assert_eq!(stored["id"], edge_id);
        assert_eq!(stored["source_id"], concept_id);
        assert_eq!(stored["target_id"], source_id);
        assert_eq!(stored["relation"], "introduced_by");
        assert_eq!(stored["weight"], json!(effective), "case {index}");
        assert_eq!(stored["weight"], response["weight"]);
    }
}
