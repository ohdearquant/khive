use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::json;

use crate::KnowledgePack;

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

#[tokio::test]
async fn real_registry_lint_reports_graph_records_and_stats_only_omits_violations() {
    let (_runtime, registry) = fixture();
    let created = registry
        .dispatch("create", json!({"kind":"project","name":"orphan project"}))
        .await
        .unwrap();
    let report = registry
        .dispatch("knowledge.lint", json!({"severity":"warn"}))
        .await
        .unwrap();
    assert_eq!(report["stats"]["total_entities"], 1);
    assert_eq!(report["stats"]["entity_kinds"]["project"], 1);
    assert_eq!(report["stats"]["density_histogram"]["0"], 1);
    assert_eq!(report["stats"]["avg_density"], 0.0);
    let violations = report["violations"].as_array().unwrap();
    assert!(violations.iter().any(|row| row["rule"] == "orphan-entity"));
    assert!(violations
        .iter()
        .any(|row| row["rule"] == "missing-entity-type"));
    for row in violations {
        assert_eq!(row["substrate"], "entity");
        assert_eq!(row["entity_id"], created["id"]);
    }
    assert!(report.get("fixes").is_none());
    assert!(report["summary"].get("fixed").is_none());
    let stats = registry
        .dispatch("knowledge.lint", json!({"rules":["stats-only"]}))
        .await
        .unwrap();
    assert_eq!(stats.as_object().unwrap().len(), 1);
    assert_eq!(stats["stats"], report["stats"]);
    let after = registry
        .dispatch("get", json!({"id":created["id"]}))
        .await
        .unwrap();
    assert_eq!(after["name"], "orphan project");
}

#[tokio::test]
async fn knowledge_corpus_rows_are_outside_graph_lint_population() {
    let (_runtime, registry) = fixture();
    registry.dispatch("knowledge.upsert_atoms", json!({"atoms":[{
        "slug":"corpus-only","name":"Corpus only",
        "content":"One two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one."
    }]})).await.unwrap();
    let report = registry
        .dispatch("knowledge.lint", json!({"severity":"info"}))
        .await
        .unwrap();
    assert_eq!(report["stats"]["total_entities"], 0);
    assert_eq!(report["stats"]["total_notes"], 0);
    assert_eq!(report["stats"]["total_edges"], 0);
    assert_eq!(report["stats"]["avg_density"], 0.0);
    assert_eq!(report["violations"], json!([]));
}

#[tokio::test]
async fn dispatch_preserves_strict_parameter_errors_and_metadata() {
    let (_runtime, registry) = fixture();
    for params in [
        json!({"rules":null}),
        json!({"rules":[]}),
        json!({"fix":null}),
        json!({"severity":"bogus"}),
        json!({"rules":["stats-only"],"fix":true}),
    ] {
        assert!(matches!(
            registry.dispatch("knowledge.lint", params).await,
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    let handler = registry
        .all_verbs()
        .into_iter()
        .find(|handler| handler.name == "knowledge.lint")
        .unwrap();
    let mut names: Vec<_> = handler.params.iter().map(|param| param.name).collect();
    names.sort();
    assert_eq!(names, ["fix", "rules", "severity"]);
    assert!(handler.params.iter().all(|param| !param.required));
    assert!(handler.description.contains("graph"));
}

#[derive(Debug)]
struct RefuseOrdinaryMutation;
impl Gate for RefuseOrdinaryMutation {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(
            if matches!(request.verb.as_str(), "create" | "update" | "delete") {
                GateDecision::deny("ordinary mutation refused")
            } else {
                GateDecision::allow()
            },
        )
    }
}

#[tokio::test]
async fn fix_true_with_no_candidates_keeps_empty_receipts_and_frozen_report() {
    let (_runtime, registry) = fixture();
    registry
        .dispatch(
            "create",
            json!({"kind":"document","name":"orphan document"}),
        )
        .await
        .unwrap();
    let observed = registry
        .dispatch("knowledge.lint", json!({"rules":["orphan-entity"]}))
        .await
        .unwrap();
    let fixed = registry
        .dispatch(
            "knowledge.lint",
            json!({"rules":["orphan-entity"],"fix":true}),
        )
        .await
        .unwrap();
    assert_eq!(fixed["violations"], observed["violations"]);
    assert_eq!(fixed["stats"], observed["stats"]);
    assert_eq!(fixed["summary"]["fixed"], 0);
    assert_eq!(fixed["fixes"], json!([]));
}

#[tokio::test]
async fn duplicate_fix_requires_the_ordinary_create_gate() {
    let (runtime, initial) = fixture();
    for _ in 0..2 {
        initial
            .dispatch("create", json!({"kind":"document","name":"same"}))
            .await
            .unwrap();
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(RefuseOrdinaryMutation));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let denied = builder.build().unwrap();
    let observed = denied
        .dispatch("knowledge.lint", json!({"rules":["duplicate-name"]}))
        .await
        .unwrap();
    assert_eq!(observed["summary"]["total"], 1);
    let error = denied
        .dispatch(
            "knowledge.lint",
            json!({"rules":["duplicate-name"],"fix":true}),
        )
        .await
        .unwrap_err();
    let wire = khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(wire["domain_disposition"], "unknown");
    assert_eq!(wire["retryable"], false);
    assert_eq!(wire["details"]["lint_fix"]["outcome"], "failed");
    assert_eq!(wire["details"]["lint_fix"]["confirmed_receipts"], json!([]));
    let after = denied
        .dispatch("knowledge.lint", json!({"rules":["stats-only"]}))
        .await
        .unwrap();
    assert_eq!(after["stats"]["total_notes"], 0);
    assert_eq!(after["stats"]["total_entities"], 2);
}

#[tokio::test]
async fn duplicate_fix_proposes_a_question_in_the_observed_namespace_without_merging() {
    let (_runtime, registry) = fixture();
    let identity = khive_runtime::RequestIdentity {
        namespace: "alpha".into(),
        actor_id: Some("lint-tester".into()),
        ..Default::default()
    };
    let mut member_ids = Vec::new();
    for _ in 0..2 {
        let created = registry
            .dispatch_with_identity(
                "create",
                json!({"kind":"document","name":"same"}),
                Some(identity.clone()),
            )
            .await
            .unwrap();
        member_ids.push(created["id"].as_str().unwrap().to_string());
    }
    let fixed = registry
        .dispatch_with_identity(
            "knowledge.lint",
            json!({"rules":["duplicate-name"],"fix":true}),
            Some(identity.clone()),
        )
        .await
        .unwrap();
    assert_eq!(fixed["summary"]["total"], 1);
    assert_eq!(fixed["summary"]["fixed"], 1);
    assert_eq!(fixed["stats"]["total_entities"], 2);
    assert_eq!(fixed["stats"]["total_notes"], 0);
    assert_eq!(fixed["fixes"].as_array().unwrap().len(), 1);
    let receipt = &fixed["fixes"][0];
    assert_eq!(receipt["action"], "propose_duplicate_merge");
    assert_eq!(receipt["status"], "committed");
    assert_eq!(receipt["violation_index"], 0);
    let question_id = receipt["question_note_id"].as_str().unwrap();
    assert_eq!(
        uuid::Uuid::parse_str(question_id).unwrap().to_string(),
        question_id
    );
    let question = registry
        .dispatch_with_identity("get", json!({"id":question_id}), Some(identity.clone()))
        .await
        .unwrap();
    assert_eq!(question["namespace"], "alpha");
    assert_eq!(question["kind"], "question");
    for id in &member_ids {
        assert!(question["content"].as_str().unwrap().contains(id));
    }
    let after = registry
        .dispatch_with_identity(
            "knowledge.lint",
            json!({"rules":["stats-only"]}),
            Some(identity.clone()),
        )
        .await
        .unwrap();
    assert_eq!(after["stats"]["total_entities"], 2);
    assert_eq!(after["stats"]["total_notes"], 1);
    for id in &member_ids {
        let kept = registry
            .dispatch_with_identity("get", json!({"id":id}), Some(identity.clone()))
            .await
            .unwrap();
        assert_eq!(kept["name"], "same");
    }
    let local = registry
        .dispatch("knowledge.lint", json!({"rules":["stats-only"]}))
        .await
        .unwrap();
    assert_eq!(local["stats"]["total_notes"], 0);
    assert_eq!(local["stats"]["total_entities"], 0);
}
