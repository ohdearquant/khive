#[tokio::test]
async fn conflict_indexes_cross_mcp_and_preserve_only_unrelated_writes() -> anyhow::Result<()> {
    let config = RuntimeConfig {
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
        packs: vec!["kg".into()],
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        ..RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::new(config)?;
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let server = KhiveMcpServer::new(runtime)?;
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    let a = ok_one(
        &client,
        r#"create(kind="concept", name="Conflict original", skip_dedup_check=true)"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="concept", name="Unrelated original", skip_dedup_check=true)"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap();
    let b_id = b["id"].as_str().unwrap();
    let result = call(&client, "request", json!({
        "ops": format!(r#"[update(id="{a_id}", name="forbidden first"), update(id="{b_id}", name="Unrelated updated"), update(id="{a_id}", name="forbidden second")]"#),
        "format":"json", "presentation":"verbose"
    })).await?;
    let envelope: Value = serde_json::from_str(&first_text(&result))?;
    for index in [0, 2] {
        assert_eq!(envelope["results"][index]["ok"], false);
        assert_eq!(envelope["results"][index]["conflict_ops"], json!([0, 2]));
        assert_eq!(
            envelope["results"][index]["domain_disposition"],
            "not_committed"
        );
        assert!(envelope["results"][index]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("writes overlap"));
    }
    assert_eq!(envelope["results"][1]["ok"], true);
    assert!(envelope["results"][1].get("conflict_ops").is_none());
    assert_eq!(
        envelope["summary"],
        json!({"total":3,"succeeded":1,"failed":2,"aborted":0})
    );
    let a = ok_one(&client, &format!(r#"get(id="{a_id}")"#)).await?;
    let b = ok_one(&client, &format!(r#"get(id="{b_id}")"#)).await?;
    assert_eq!(a["name"], "Conflict original");
    assert_eq!(b["name"], "Unrelated updated");
    Ok(())
}
