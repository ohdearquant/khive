//! Every bulk link is validated, including entries sharing a natural key.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use serde_json::{json, Value};

fn registry() -> VerbRegistry {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        wal_ceiling_source: WalCeilingSource::Default,
        disk_guard_config: None,
        disk_guard_environment: Default::default(),
        volume_lock_dir: None,
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into(), "gtd".into()],
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
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and GTD registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn node(registry: &VerbRegistry, kind: &str, name: &str) -> String {
    let (verb, params, field) = if kind == "task" {
        ("gtd.assign", json!({"title": name}), "full_id")
    } else {
        (
            "create",
            json!({"kind": kind, "name": name, "skip_dedup_check": true}),
            "id",
        )
    };
    registry
        .dispatch(verb, params)
        .await
        .expect("create endpoint")[field]
        .as_str()
        .expect("full endpoint id")
        .to_owned()
}

async fn pair(registry: &VerbRegistry, kind: &str) -> (String, String) {
    (
        node(registry, kind, "Left").await,
        node(registry, kind, "Right").await,
    )
}

fn link(source: &str, target: &str, relation: &str) -> Value {
    json!({"source_id": source, "target_id": target, "relation": relation,
        "weight": 0.4, "metadata": {"phase": "first"}})
}

fn batch(links: Vec<Value>, atomic: Option<bool>) -> Value {
    let mut params = json!({"links": links, "verbose": true});
    if let Some(atomic) = atomic {
        params["atomic"] = json!(atomic);
    }
    params
}

fn malformed_payloads() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        ("metadata", json!(false), "metadata must be a JSON object"),
        ("metadata", json!("text"), "metadata must be a JSON object"),
        ("metadata", json!([]), "metadata must be a JSON object"),
        (
            "metadata",
            json!({"optional": "false"}),
            "metadata.optional",
        ),
        (
            "metadata",
            json!({"dependency_kind": false}),
            "dependency_kind must be a string",
        ),
        (
            "metadata",
            json!({"dependency_kind": "unknown"}),
            "unknown dependency_kind",
        ),
        ("weight", json!(-0.1), "edge weight"),
        ("weight", json!(1.1), "edge weight"),
    ]
}

async fn edges(registry: &VerbRegistry, source: &str) -> Vec<Value> {
    let result = registry
        .dispatch("list", json!({"kind": "edge", "source_id": source}))
        .await
        .expect("read stored edges");
    result["items"].as_array().expect("edge items").clone()
}

#[tokio::test]
async fn atomic_duplicate_validation_refuses_before_create_or_replace() {
    let registry = registry();
    for atomic in [None, Some(true)] {
        for (field, invalid, diagnostic) in malformed_payloads() {
            for existing in [false, true] {
                let (source, target) = pair(&registry, "document").await;
                let good = link(&source, &target, "depends_on");
                let before = if existing {
                    let mut original = good.clone();
                    original["weight"] = json!(0.2);
                    original["metadata"] = json!({"phase": "original"});
                    Some(
                        registry
                            .dispatch("link", original)
                            .await
                            .expect("seed edge"),
                    )
                } else {
                    None
                };
                let mut bad = good.clone();
                bad[field] = invalid.clone();
                let error = registry
                    .dispatch("link", batch(vec![good, bad], atomic))
                    .await
                    .expect_err("malformed duplicate must refuse atomic batch");
                assert!(error.to_string().contains(diagnostic), "{error}");
                let stored = edges(&registry, &source).await;
                if let Some(before) = before {
                    assert_eq!(stored.len(), 1);
                    let after = registry
                        .dispatch("get", json!({"id": before["id"]}))
                        .await
                        .expect("read unchanged edge");
                    for field in ["id", "weight", "metadata", "deleted_at"] {
                        assert_eq!(after[field], before[field], "{field}");
                    }
                } else {
                    assert!(stored.is_empty(), "{stored:?}");
                }
            }
        }
    }
}

#[tokio::test]
async fn non_atomic_duplicates_report_invalid_entries_in_both_orders() {
    let registry = registry();
    for (field, invalid, diagnostic) in malformed_payloads() {
        for valid_first in [false, true] {
            let (source, target) = pair(&registry, "document").await;
            let good = link(&source, &target, "depends_on");
            let mut bad = good.clone();
            bad[field] = invalid.clone();
            let entries = if valid_first {
                vec![good, bad]
            } else {
                vec![bad, good]
            };
            let result = registry
                .dispatch("link", batch(entries, Some(false)))
                .await
                .expect("best-effort result");
            assert_eq!(result["attempted"], 2, "{result}");
            assert_eq!(result["created"], 1, "{result}");
            assert_eq!(result["failed"], 1, "{result}");
            assert_eq!(result["skipped"], 0, "{result}");
            assert_eq!(
                result["errors"][0]["index"],
                if valid_first { 1 } else { 0 }
            );
            assert!(
                result["errors"][0]["error"]
                    .as_str()
                    .unwrap()
                    .contains(diagnostic),
                "{result}"
            );
            let stored = edges(&registry, &source).await;
            assert_eq!(stored.len(), 1);
            let edge = registry
                .dispatch("get", json!({"id": stored[0]["id"]}))
                .await
                .expect("read successful edge");
            assert_eq!(edge["weight"], 0.4);
            assert_eq!(edge["metadata"]["phase"], "first");
            assert_eq!(edge["metadata"]["dependency_kind"], "normative");
        }
    }
}

#[tokio::test]
async fn refused_non_atomic_link_does_not_suppress_explicit_resurrection() {
    let registry = registry();
    let (source, target) = pair(&registry, "document").await;
    let original = link(&source, &target, "depends_on");
    let created = registry
        .dispatch("link", original.clone())
        .await
        .expect("seed edge");
    registry
        .dispatch("delete", json!({"id": created["id"], "hard": false}))
        .await
        .expect("soft-delete edge");
    let tombstone = registry
        .dispatch("get", json!({"id": created["id"], "include_deleted": true}))
        .await
        .expect("read tombstone");
    assert!(!tombstone["deleted_at"].is_null());
    let mut refused = original.clone();
    refused["resurrect"] = json!(false);
    let mut accepted = original;
    accepted["resurrect"] = json!(true);
    accepted["metadata"] = json!({"phase": "restored"});
    let result = registry
        .dispatch("link", batch(vec![refused, accepted], Some(false)))
        .await
        .expect("best-effort restore result");
    assert_eq!(result["attempted"], 2, "{result}");
    assert_eq!(result["failed"], 1, "{result}");
    assert_eq!(result["resurrected"], 1, "{result}");
    assert_eq!(result["created"], 0, "{result}");
    assert_eq!(result["skipped"], 0, "{result}");
    assert_eq!(result["errors"][0]["index"], 0);
    assert!(result["errors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("resurrect=true"));
    let restored = registry
        .dispatch("get", json!({"id": created["id"]}))
        .await
        .expect("read restored edge");
    assert_eq!(restored["id"], created["id"]);
    assert!(restored["deleted_at"].is_null());
    assert_eq!(restored["metadata"]["phase"], "restored");
    assert_eq!(edges(&registry, &source).await.len(), 1);
}

#[tokio::test]
async fn valid_duplicates_preserve_first_payload_and_null_defaults() {
    let registry = registry();
    for atomic in [None, Some(true), Some(false)] {
        for metadata in [
            None,
            Some(Value::Null),
            Some(json!({})),
            Some(json!({"optional": false})),
            Some(json!({"custom": [1, "open"]})),
        ] {
            for null_weight in [false, true] {
                let (source, target) = pair(&registry, "document").await;
                let mut first = link(&source, &target, "depends_on");
                match &metadata {
                    Some(value) => {
                        first["metadata"] = value.clone();
                    }
                    None => {
                        first.as_object_mut().unwrap().remove("metadata");
                    }
                }
                if null_weight {
                    first["weight"] = Value::Null;
                } else {
                    first.as_object_mut().unwrap().remove("weight");
                }
                let mut duplicate = first.clone();
                duplicate["weight"] = json!(0.8);
                duplicate["metadata"] = json!({"later": true});
                let result = registry
                    .dispatch("link", batch(vec![first, duplicate], atomic))
                    .await
                    .expect("valid duplicates");
                assert_eq!(result["created"], 1, "{result}");
                assert_eq!(result["skipped"], 1, "{result}");
                assert_eq!(result["failed"], 0, "{result}");
                let stored = edges(&registry, &source).await;
                assert_eq!(stored.len(), 1);
                let edge = registry
                    .dispatch("get", json!({"id": stored[0]["id"]}))
                    .await
                    .expect("read selected edge");
                assert_eq!(edge["weight"], 1.0);
                let mut expected = metadata
                    .clone()
                    .filter(|value| !value.is_null())
                    .unwrap_or(json!({}));
                expected["dependency_kind"] = json!("normative");
                assert_eq!(edge["metadata"], expected);
            }
        }
    }
}

#[tokio::test]
async fn reversed_symmetric_duplicates_still_coalesce() {
    let registry = registry();
    for atomic in [None, Some(true), Some(false)] {
        let (source, target) = pair(&registry, "concept").await;
        let first = link(&source, &target, "competes_with");
        let mut duplicate = link(&target, &source, "competes_with");
        duplicate["metadata"] = json!({"phase": "later"});
        let result = registry
            .dispatch("link", batch(vec![first, duplicate], atomic))
            .await
            .expect("reversed symmetric duplicate");
        assert_eq!(result["created"], 1, "{result}");
        assert_eq!(result["skipped"], 1, "{result}");
        assert_eq!(result["failed"], 0, "{result}");
        assert_eq!(result["edges"].as_array().unwrap().len(), 1);
        let edge = registry
            .dispatch("get", json!({"id": result["edges"][0]["id"]}))
            .await
            .expect("read symmetric edge");
        assert_eq!(edge["metadata"]["phase"], "first");
    }
}

#[tokio::test]
async fn deduplication_keeps_the_selected_batch_dependency_cycle_guard() {
    let registry = registry();
    let (source, target) = pair(&registry, "task").await;
    let forward = link(&source, &target, "depends_on");
    let backward = link(&target, &source, "depends_on");
    let error = registry
        .dispatch(
            "link",
            batch(vec![forward.clone(), forward.clone(), backward], None),
        )
        .await
        .expect_err("selected batch must reject its two-edge cycle");
    assert!(error.to_string().contains("dependency cycle"), "{error}");
    assert!(edges(&registry, &source).await.is_empty());
    assert!(edges(&registry, &target).await.is_empty());
    let result = registry
        .dispatch("link", batch(vec![forward.clone(), forward], None))
        .await
        .expect("valid duplicate task link");
    assert_eq!(result["created"], 1, "{result}");
    assert_eq!(result["skipped"], 1, "{result}");
    assert_eq!(result["failed"], 0, "{result}");
    assert_eq!(edges(&registry, &source).await.len(), 1);
}
