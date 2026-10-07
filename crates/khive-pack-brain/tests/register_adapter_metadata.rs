use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use khive_types::Namespace;
use serde_json::{json, Value};

fn registry() -> (KhiveRuntime, VerbRegistry) {
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
        packs: vec!["kg".into(), "brain".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("private in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and brain registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("brain schema on the private memory backend");
    (runtime, registry)
}

fn active_revision() -> String {
    std::env::var("KHIVE_BRAIN_BASE_MODEL_REVISION").unwrap_or_else(|_| "base-v0".into())
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn register_adapter_rejects_non_object_metadata_without_an_artifact() {
    let (runtime, registry) = registry();
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let revision = active_revision();
    let initial = runtime
        .list_entities(&token, Some("artifact"), Some("adapter"), 20, 0)
        .await
        .expect("initial adapter artifacts");
    assert!(initial.is_empty());

    for (index, metadata) in [
        json!([]),
        json!([{"nested": true}]),
        json!(""),
        json!("ignored"),
        json!(true),
        json!(1),
        json!(1.25),
    ]
    .into_iter()
    .enumerate()
    {
        let result = registry
            .dispatch(
                "brain.register_adapter",
                json!({
                    "adapter_id": format!("invalid-metadata-{index}"),
                    "content_hash": "sha256:metadata-fixture",
                    "base_model_revision": revision,
                    "metadata": metadata,
                }),
            )
            .await;
        let artifacts = runtime
            .list_entities(&token, Some("artifact"), Some("adapter"), 20, 0)
            .await
            .expect("adapter artifacts after refusal");
        assert!(
            artifacts.is_empty(),
            "invalid metadata persisted an adapter: {metadata}"
        );
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "{metadata}: {result:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn register_adapter_preserves_optional_metadata_and_reserved_fields() {
    let (runtime, registry) = registry();
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let revision = active_revision();
    let extra = json!({"labels": ["alpha", 2, null], "nested": {"enabled": true}});
    for (index, metadata) in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({
            "content_hash": 17,
            "base_model_revision": false,
            "extra": extra,
        })),
    ]
    .into_iter()
    .enumerate()
    {
        let adapter_id = format!("valid-metadata-{index}");
        let mut params = json!({
            "adapter_id": adapter_id,
            "content_hash": "sha256:metadata-fixture",
            "base_model_revision": revision,
        });
        if let Some(metadata) = metadata {
            params["metadata"] = metadata;
        }
        let response = registry
            .dispatch("brain.register_adapter", params)
            .await
            .expect("valid metadata");
        assert_eq!(
            response,
            json!({
                "registered": true,
                "adapter_id": adapter_id,
                "content_hash": "sha256:metadata-fixture",
                "base_model_revision": revision,
            })
        );
        let artifacts = runtime
            .list_entities(&token, Some("artifact"), Some("adapter"), 20, 0)
            .await
            .expect("persisted adapter artifacts");
        assert_eq!(artifacts.len(), index + 1);
        let artifact = artifacts
            .iter()
            .find(|artifact| artifact.name == adapter_id)
            .expect("new adapter");
        let mut expected = json!({
            "content_hash": "sha256:metadata-fixture",
            "base_model_revision": revision,
        });
        if index == 3 {
            expected["extra"] = extra.clone();
        }
        assert_eq!(artifact.properties.as_ref(), Some(&expected));
    }
}
