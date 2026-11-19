use std::collections::BTreeSet;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn fixture() -> (KhiveRuntime, VerbRegistry) {
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
        packs: vec!["kg".into(), "knowledge".into()],
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("private memory runtime");
    assert!(runtime.config().db_path.is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
    let registry = builder.build().expect("serving registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn acquisitions(runtime: &KhiveRuntime) -> u64 {
    runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .acquisitions
}

async fn seed(registry: &VerbRegistry) {
    let response = registry.dispatch("knowledge.upsert_atoms", json!({
        "atoms": [
            {
                "slug": "shape-draft",
                "name": "Statusshape Draft",
                "content": "statusshape corpus fixture uses shared retrieval vocabulary to expose explicit status selection across draft and reviewed atoms without embeddings or external services in this isolated test",
                "finalized": false
            },
            {
                "slug": "shape-reviewed",
                "name": "Statusshape Reviewed",
                "content": "statusshape corpus fixture uses shared retrieval vocabulary to expose explicit status selection across draft and reviewed atoms without embeddings or external services in this isolated test",
                "finalized": true
            }
        ]
    })).await.expect("seed draft and reviewed atoms");
    assert_eq!(response["created"], 2);
}

fn slugs(response: &Value) -> BTreeSet<&str> {
    response["results"]
        .as_array()
        .expect("search results")
        .iter()
        .map(|row| row["slug"].as_str().expect("result slug"))
        .collect()
}

#[tokio::test]
async fn malformed_status_shapes_refuse_before_reader_acquisition() {
    let (runtime, registry) = fixture();
    seed(&registry).await;
    for status in [
        json!(false),
        json!(42),
        json!(0.5),
        json!({}),
        json!({"status": "draft"}),
        json!([false]),
        json!([null]),
        json!(["draft", false]),
        json!([false, "draft"]),
        json!(["reviewed", 42]),
        json!(["reviewed", {}]),
        json!(["reviewed", ["draft"]]),
    ] {
        let before = acquisitions(&runtime);
        let error = registry
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "statusshape", "status": status, "rerank": false,
                    "include_drafts": true, "exclude_status": "reviewed"
                }),
            )
            .await
            .expect_err("malformed status must not silently alter the filter");
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message)
            if message == "status must be a string or an array of strings"),
            "{status}: {error}"
        );
        assert_eq!(
            acquisitions(&runtime),
            before,
            "malformed status {status} opened a reader"
        );
    }
    let before = acquisitions(&runtime);
    let response = registry
        .dispatch(
            "knowledge.search",
            json!({
                "query": "statusshape", "status": "draft", "rerank": false
            }),
        )
        .await
        .expect("valid explicit status searches");
    assert_eq!(slugs(&response), BTreeSet::from(["shape-draft"]));
    assert!(
        acquisitions(&runtime) > before,
        "valid search exercises the reader counter"
    );
}

#[tokio::test]
async fn valid_status_shapes_preserve_normalization_and_precedence() {
    let (_runtime, registry) = fixture();
    seed(&registry).await;
    for (filters, expected) in [
        (json!({}), vec!["shape-reviewed"]),
        (json!({"status": null}), vec!["shape-reviewed"]),
        (json!({"status": "  "}), vec!["shape-reviewed"]),
        (json!({"status": []}), vec!["shape-reviewed"]),
        (json!({"status": [" ", ""]}), vec!["shape-reviewed"]),
        (json!({"status": " draft "}), vec!["shape-draft"]),
        (
            json!({"status": ["draft", "reviewed"]}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (
            json!({"status": [" reviewed ", "reviewed", ""]}),
            vec!["shape-reviewed"],
        ),
        (json!({"status": "custom-status"}), vec![]),
        (json!({"status": ["custom-status"]}), vec![]),
        (
            json!({"status": "reviewed", "include_drafts": true, "exclude_status": "reviewed"}),
            vec!["shape-reviewed"],
        ),
        (
            json!({"status": ["draft"], "include_drafts": false, "exclude_status": "draft"}),
            vec!["shape-draft"],
        ),
        (
            json!({"status": [], "include_drafts": true}),
            vec!["shape-draft", "shape-reviewed"],
        ),
        (
            json!({"status": null, "exclude_status": "reviewed"}),
            vec!["shape-draft"],
        ),
    ] {
        let mut params = json!({"query": "statusshape", "rerank": false, "limit": 10});
        params
            .as_object_mut()
            .unwrap()
            .extend(filters.as_object().unwrap().clone());
        let response = registry
            .dispatch("knowledge.search", params)
            .await
            .expect("valid status shape");
        assert_eq!(
            slugs(&response),
            expected.into_iter().collect::<BTreeSet<_>>(),
            "filters {filters}: {response}"
        );
    }
}
