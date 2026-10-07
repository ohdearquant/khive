//! Selected MCP schemas are validated through the real tool registry.

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{
    KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use serde_json::{json, Value};

fn registry() -> VerbRegistry {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into(), "tool".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("isolated in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("schema-operator".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and tool registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("tool schema on the private memory backend");
    registry
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn malformed_selected_mcp_schemas_refuse_without_registering_the_item() {
    let registry = registry();
    for (key, fallback) in [
        ("inputSchema", false),
        ("schema", false),
        ("inputSchema", true),
    ] {
        for (index, invalid) in [json!("not-an-object"), json!([]), json!(true), json!(42)]
            .into_iter()
            .enumerate()
        {
            let name = format!("invalid-{key}-{fallback}-{index}");
            let mut tool = json!({"name": name});
            tool[key] = invalid;
            if fallback {
                tool["schema"] = json!({"type": "object"});
            }
            let error = registry
                .dispatch(
                    "tool.ingest",
                    json!({"source": "mcp", "server": "fixture", "tools": [tool]}),
                )
                .await
                .expect_err("a malformed selected schema must refuse");
            assert!(
                matches!(&error, RuntimeError::InvalidInput(message)
                if message == &format!("schema must be an object for tool {name:?}")),
                "{error:?}"
            );
            let listed = registry.dispatch("tool.list", json!({})).await.unwrap();
            assert_eq!(listed["ok"], true);
            assert_eq!(listed["count"], 0);
            assert_eq!(listed["tools"], json!([]));
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn accepted_mcp_schemas_preserve_exact_selection_and_storage() {
    let registry = registry();
    let primary = json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]});
    let fallback = json!({"type": "object", "properties": {"limit": {"type": "integer"}}});
    let cases = [
        ("omitted", json!({}), Value::Null),
        ("primary-null", json!({"inputSchema": null}), Value::Null),
        ("fallback-null", json!({"schema": null}), Value::Null),
        ("empty-object", json!({"inputSchema": {}}), json!({})),
        ("primary", json!({"inputSchema": primary}), primary.clone()),
        ("fallback", json!({"schema": fallback}), fallback.clone()),
        (
            "both-objects",
            json!({"inputSchema": primary, "schema": fallback}),
            primary.clone(),
        ),
        (
            "primary-null-shadows-fallback",
            json!({"inputSchema": null, "schema": fallback}),
            Value::Null,
        ),
        (
            "primary-object-shadows-invalid",
            json!({"inputSchema": primary, "schema": false}),
            primary.clone(),
        ),
        (
            "primary-null-shadows-invalid",
            json!({"inputSchema": null, "schema": false}),
            Value::Null,
        ),
    ];
    for (index, (name, mut tool, expected)) in cases.into_iter().enumerate() {
        tool["name"] = json!(name);
        let result = registry
            .dispatch(
                "tool.ingest",
                json!({"source": "mcp", "server": "fixture", "tools": [tool]}),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            json!({"ok": true, "source": "mcp", "registered": 1, "existing": 0, "names": [name]})
        );
        let described = registry
            .dispatch("tool.describe", json!({"tool": name}))
            .await
            .unwrap();
        assert_eq!(described["ok"], true);
        assert_eq!(described["tool"]["name"], name);
        assert_eq!(described["tool"]["schema"], expected);
        assert_eq!(described["tool"]["source"], "mcp:fixture");
        let properties = described["tool"]["properties"].as_object().unwrap();
        assert_eq!(
            properties.get("schema"),
            if expected.is_null() {
                None
            } else {
                Some(&expected)
            }
        );
        let listed = registry.dispatch("tool.list", json!({})).await.unwrap();
        assert_eq!(listed["count"], index + 1);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn invalid_later_schema_retains_earlier_registration_and_stops_the_loop() {
    let registry = registry();
    let schema = json!({"type": "object", "properties": {"query": {"type": "string"}}});
    let error = registry
        .dispatch(
            "tool.ingest",
            json!({
                "source": "mcp", "server": "fixture", "tools": [
                    {"name": "before", "inputSchema": schema},
                    {"name": "invalid", "inputSchema": false},
                    {"name": "after", "inputSchema": {}}
                ]
            }),
        )
        .await
        .expect_err("the second entry must refuse");
    assert!(
        matches!(&error, RuntimeError::InvalidInput(message)
        if message == "schema must be an object for tool \"invalid\""),
        "{error:?}"
    );
    let listed = registry.dispatch("tool.list", json!({})).await.unwrap();
    assert_eq!(listed["ok"], true);
    assert_eq!(listed["count"], 1);
    let tools = listed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "before");
    let described = registry
        .dispatch("tool.describe", json!({"tool": "before"}))
        .await
        .unwrap();
    assert_eq!(described["tool"]["schema"], schema);
}
