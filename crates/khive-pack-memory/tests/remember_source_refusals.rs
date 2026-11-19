//! Source-reference refusals retain useful operands without exposing credentials.

#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};

async fn fixture() -> (VerbRegistry, String, String) {
    let runtime = receipt_credentials::with_receipt_credentials(
        KhiveRuntime::memory().expect("private in-memory runtime"),
    );
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("memory-source-fixture".into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    let source = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Reference source", "skip_dedup_check": true}),
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let seed = registry
        .dispatch(
            "memory.remember",
            json!({"content": "Existing sourced memory", "memory_type": "semantic", "source_id": source}),
        )
        .await
        .unwrap();
    assert!(seed["edge_id"].is_string(), "populated annotation fixture");
    let seed_id = seed["id"].as_str().unwrap().to_owned();
    (registry, source, seed_id)
}

async fn snapshot(registry: &VerbRegistry) -> Value {
    let notes = registry
        .dispatch("list", json!({"kind": "memory", "limit": 100}))
        .await
        .unwrap();
    let edges = registry
        .dispatch("list", json!({"kind": "edge", "limit": 100}))
        .await
        .unwrap();
    json!({"notes": notes, "edges": edges})
}

async fn refusal(registry: &VerbRegistry, field: &str, raw: &str, expected: String, secret: bool) {
    let before = snapshot(registry).await;
    let mut args = json!({"content": "Refused memory", "memory_type": "semantic"});
    args[field] = json!(raw);
    let error = registry
        .dispatch("memory.remember", args)
        .await
        .unwrap_err();
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("source refusal changed error class: {error:?}");
    };
    if secret {
        assert!(
            !message.contains(raw),
            "source refusal exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"));
    }
    assert_eq!(message, &expected);
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({"kind": "runtime_error", "message": format!("invalid input: {expected}"), "domain_disposition": "not_committed"})
    );
    assert_eq!(
        snapshot(registry).await,
        before,
        "refusal changed notes or edges"
    );
}

#[tokio::test]
async fn malformed_sources_mask_credentials_for_both_parameter_names() {
    let (registry, _, _) = fixture().await;
    let credential = format!("sk-proj-{}", "A".repeat(80));
    for field in ["source_id", "source"] {
        for (raw, shown, secret) in [
            ("not-a-source", "not-a-source", false),
            (credential.as_str(), "***MASKED***", true),
        ] {
            refusal(
                &registry,
                field,
                raw,
                format!("source_id {shown:?} is not a valid UUID or 8-char short ID"),
                secret,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn missing_source_prefix_keeps_its_operand_and_writes_nothing() {
    let (registry, source, seed) = fixture().await;
    let missing = (0u32..)
        .map(|value| format!("{value:08x}"))
        .find(|prefix| !source.starts_with(prefix) && !seed.starts_with(prefix))
        .unwrap();
    for field in ["source_id", "source"] {
        refusal(
            &registry,
            field,
            &missing,
            format!("source_id {missing:?}: no record matches this prefix"),
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn full_and_short_sources_keep_annotation_targets_for_both_parameter_names() {
    let (registry, source, _) = fixture().await;
    for field in ["source_id", "source"] {
        for source_ref in [source.as_str(), &source[..8]] {
            let mut args = json!({"content": format!("Accepted {field} {source_ref}"), "memory_type": "semantic"});
            args[field] = json!(source_ref);
            let result = registry.dispatch("memory.remember", args).await.unwrap();
            let edge = registry
                .dispatch("get", json!({"id": result["edge_id"]}))
                .await
                .unwrap();
            assert_eq!(edge["source_id"], result["id"]);
            assert_eq!(edge["target_id"], source);
            assert_eq!(edge["relation"], "annotates");
        }
    }
}
