#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

const MEMORY_TYPES: &str = "episodic | semantic";
const FUSION_STRATEGIES: &str =
    "\"rrf\", \"weighted\", \"union\", \"vector_only\", \"keyword_only\"";
const FEEDBACK_SIGNALS: &str = "useful | not_useful | wrong | explicit_positive | explicit_negative | implicit_positive | implicit_negative | correction";

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("memory-refusal-fixture".into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new_with_index_role(
        receipt_credentials::with_receipt_credentials(runtime),
        false,
    ));
    builder.build().expect("registry")
}

fn refused_values() -> [(String, &'static str); 2] {
    [
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

async fn assert_refusal(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    raw: &str,
    expected: String,
) {
    let error = registry
        .dispatch(verb, args)
        .await
        .expect_err("closed-set refusal");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("{verb} changed its refusal variant");
    };
    if raw != "bogus" {
        assert!(
            !message.contains(raw),
            "{verb} exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "{verb} omitted the mask");
    }
    assert_eq!(message, &expected, "{verb} diagnostic");
    let projected = runtime_error_value(error, DomainDisposition::NotCommitted);
    assert_eq!(
        projected,
        json!({
            "kind": "runtime_error",
            "message": format!("invalid input: {expected}"),
            "domain_disposition": "not_committed"
        })
    );
}

async fn remember(registry: &VerbRegistry, memory_type: &str) -> Value {
    registry
        .dispatch(
            "memory.remember",
            json!({"content":"memory refusal orchard", "memory_type":memory_type, "namespace":"local"}),
        )
        .await
        .expect("accepted memory type")
}

#[tokio::test]
async fn remember_masks_refused_memory_types_and_preserves_valid_writes() {
    let registry = fixture();
    for (raw, shown) in refused_values() {
        assert_refusal(
            &registry,
            "memory.remember",
            json!({"content":"must not be stored", "memory_type":raw, "namespace":"local"}),
            &raw,
            format!("unknown memory_type {shown:?}; valid: {MEMORY_TYPES}"),
        )
        .await;
    }
    let listed = registry
        .dispatch("list", json!({"kind":"memory", "namespace":"local"}))
        .await
        .unwrap();
    assert!(listed["items"]
        .as_array()
        .expect("memory list items")
        .is_empty());
    assert_eq!(listed["has_more"], false);
    for memory_type in ["episodic", "semantic"] {
        let created = remember(&registry, memory_type).await;
        let stored = registry
            .dispatch("get", json!({"id":created["id"]}))
            .await
            .unwrap();
        assert_eq!(stored["properties"]["memory_type"], memory_type);
    }
}

#[tokio::test]
async fn recall_masks_refused_types_and_strategies_without_changing_accepted_sets() {
    let registry = fixture();
    let created = remember(&registry, "semantic").await;
    for (raw, shown) in refused_values() {
        for parameter in ["memory_type", "fusion_strategy"] {
            let mut args = json!({"query":"orchard", "namespace":"local"});
            args[parameter] = json!(raw);
            let expected = if parameter == "memory_type" {
                format!("unknown memory_type {shown:?}; valid: {MEMORY_TYPES}")
            } else {
                format!("invalid fusion_strategy {shown:?}: must be one of {FUSION_STRATEGIES}")
            };
            assert_refusal(&registry, "memory.recall", args, &raw, expected).await;
        }
    }
    for strategy in ["rrf", "weighted", "union", "vector_only", "keyword_only"] {
        let hits = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query":"orchard", "namespace":"local", "memory_type":"semantic",
                    "fusion_strategy":strategy, "min_score":0.0
                }),
            )
            .await
            .expect("accepted fusion strategy");
        let hits = hits.as_array().expect("recall hits");
        if strategy == "keyword_only" {
            assert!(
                hits.iter().any(|hit| hit["id"] == created["id"]),
                "positive lexical retrieval"
            );
        }
    }
}

#[tokio::test]
async fn feedback_masks_refused_signals_and_accepts_semantic_and_legacy_signals() {
    let registry = fixture();
    let created = remember(&registry, "semantic").await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &registry,
            "memory.feedback",
            json!({"target_id":created["id"], "signal":raw}),
            &raw,
            format!("memory.feedback: invalid signal {shown:?}; valid: {FEEDBACK_SIGNALS}"),
        )
        .await;
    }
    for signal in [
        "useful",
        "not_useful",
        "wrong",
        "explicit_positive",
        "explicit_negative",
        "implicit_positive",
        "implicit_negative",
        "correction",
    ] {
        let result = registry
            .dispatch(
                "memory.feedback",
                json!({"target_id":created["id"], "signal":signal}),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            json!({"ok":true, "target_id":created["id"], "signal":signal})
        );
    }
}

#[tokio::test]
async fn memory_update_retains_the_shared_plain_refusal_and_existing_row() {
    let registry = fixture();
    let created = remember(&registry, "episodic").await;
    let before = registry
        .dispatch("get", json!({"id":created["id"]}))
        .await
        .unwrap();
    assert_refusal(
        &registry,
        "update",
        json!({"id":created["id"], "properties":{"memory_type":"bogus"}}),
        "bogus",
        format!("unknown memory_type \"bogus\"; valid: {MEMORY_TYPES}"),
    )
    .await;
    let after = registry
        .dispatch("get", json!({"id":created["id"]}))
        .await
        .unwrap();
    assert_eq!(after, before);
}
