//! Refused enum values retain useful diagnostics without echoing credentials.
use std::collections::HashMap;

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "brain".into()],
        actor_id: Some("brain-refusal-fixture".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("brain-refusal-fixture".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
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
    params: Value,
    raw: &str,
    expected: String,
) {
    let error = registry
        .dispatch(verb, params)
        .await
        .expect_err("invalid enum");
    let RuntimeError::InvalidInput(ref message) = error else {
        panic!("{verb} changed its error kind: {error:?}");
    };
    assert_eq!(message, &expected, "{verb}");
    let displayed = error.to_string();
    let projected = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(projected["kind"], "runtime_error");
    assert_eq!(projected["message"], displayed);
    if raw != "bogus" {
        assert!(!projected.to_string().contains(raw), "credential leaked");
        assert!(displayed.contains("***MASKED***"));
    }
}

#[tokio::test]
async fn profile_lifecycle_refusals_mask_credentials_and_preserve_filters() {
    let registry = fixture();
    let before = registry
        .dispatch("brain.profiles", json!({}))
        .await
        .unwrap();
    for (raw, shown) in refused_values() {
        assert_refusal(
            &registry,
            "brain.profiles",
            json!({"lifecycle": raw}),
            &raw,
            format!(
                "invalid lifecycle {shown:?}; expected one of 'active', 'inactive', 'archived'"
            ),
        )
        .await;
    }
    for lifecycle in ["active", "inactive", "archived"] {
        let result = registry
            .dispatch("brain.profiles", json!({"lifecycle": lifecycle}))
            .await
            .expect("valid lifecycle");
        assert!(result["profiles"].is_array());
    }
    assert_eq!(
        registry
            .dispatch("brain.profiles", json!({}))
            .await
            .unwrap(),
        before
    );
}

async fn feedback_refusals(verb: &str) {
    let registry = fixture();
    let target = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "refusal target"}),
        )
        .await
        .expect("target")["id"]
        .clone();
    registry
        .dispatch("brain.create_profile", json!({"name": "refusal-profile"}))
        .await
        .expect("profile");
    let params = |signal: &str| {
        let mut args = json!({
            "target_id": target,
            "signal": signal,
            "served_by_profile_id": "refusal-profile",
        });
        if verb == "brain.auto_feedback" {
            args["query"] = json!("retrieval quality");
            args["results"] = json!([{"id": target}]);
        }
        args
    };
    for (raw, shown) in refused_values() {
        assert_refusal(
            &registry,
            verb,
            params(&raw),
            &raw,
            format!(
                "unknown signal {shown:?}; valid: useful | not_useful | wrong | \
                 explicit_positive | explicit_negative | implicit_positive | \
                 implicit_negative | correction"
            ),
        )
        .await;
    }
    let accepted = registry
        .dispatch(verb, params("useful"))
        .await
        .expect("valid feedback");
    assert_eq!(accepted["emitted"], true);
    assert_eq!(accepted["signal"], "useful");
}

#[tokio::test]
async fn feedback_signal_refusals_mask_credentials_and_preserve_valid_feedback() {
    feedback_refusals("brain.feedback").await;
}

#[tokio::test]
async fn auto_feedback_signal_refusals_share_masked_diagnostics() {
    feedback_refusals("brain.auto_feedback").await;
}
