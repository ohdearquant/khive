use super::*;
use crate::KnowledgePack;
use khive_brain_core::{FeedbackSignal, SectionPosteriorState, SectionType};
use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::VerbRegistryBuilder;
use std::collections::HashMap;

#[tokio::test]
async fn compose_without_profile_binding_uses_pack_local_tier3_weights() {
    let runtime = rt_with_fake_embedder();
    let pack = KnowledgePack::new(runtime.clone());
    let mut state = SectionPosteriorState::default();
    for _ in 0..80 {
        crate::knowledge::section_feedback::on_section_feedback(
            &mut state,
            &[
                (SectionType::Formalism, FeedbackSignal::Useful),
                (SectionType::OperationalGuidance, FeedbackSignal::Wrong),
            ],
        );
    }
    state.exploration_epoch = 0;
    let expected: HashMap<String, f32> = state
        .deterministic_weights()
        .into_iter()
        .map(|(kind, weight)| (kind.as_str().to_owned(), weight as f32))
        .collect();
    assert!(expected["formalism"] > expected["operational_guidance"]);
    pack.section_posteriors
        .lock()
        .unwrap()
        .insert("local".into(), state);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    builder.register(pack);
    let registry = builder.build().unwrap();
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());

    let resolved = registry
        .dispatch(
            "brain.resolve",
            json!({
                "namespace":"local", "consumer_kind":"knowledge_compose"
            }),
        )
        .await
        .expect("a loaded consumer without a profile resolves successfully");
    assert_eq!(resolved["matched_binding"], false);
    assert_eq!(
        resolved.get("resolved_profile_id"),
        Some(&serde_json::Value::Null)
    );
    registry.dispatch("knowledge.upsert_atoms", json!({"atoms":[{
        "slug":"tier3-compose", "name":"Tier three compose", "content":
        "retrieval context uses formal and operational guidance to choose useful source evidence while preserving independent section weights for deterministic briefing assembly across requests"
    }]})).await.unwrap();
    registry.dispatch("knowledge.edit", json!({
        "id":"tier3-compose", "sections":[
            {"section_type":"formalism", "heading":"Formal rule", "content":
             "retrieval context selects useful source evidence by a formal ranking rule that keeps each candidate traceable to its source document"},
            {"section_type":"operational_guidance", "heading":"Operational rule", "content":
             "retrieval context selects useful source evidence by an operational ranking rule that keeps each step useful for the active task"}
        ]
    })).await.unwrap();
    let response = registry
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids":["tier3-compose"], "query":"retrieval context source evidence formal operational guidance section weights produce a clear useful briefing for this request",
                "max_tokens":1500, "explain":true
            }),
        )
        .await
        .unwrap();
    assert_eq!(response["status"], "ok");
    let data = &response["data"];
    let sections = data["sections"].as_array().expect("embedded section path");
    assert_eq!(
        sections.len(),
        2,
        "both section types must exercise compose: {data}"
    );
    assert_eq!(data["section_count"], 2);
    for kind in ["formalism", "operational_guidance"] {
        let section = sections
            .iter()
            .find(|section| section["section_type"] == kind)
            .expect("each tuned section type must reach the returned briefing");
        let rounded = (expected[kind] * 10_000.0).round() / 10_000.0;
        assert_eq!(
            section["breakdown"]["type_weight"],
            json!(rounded),
            "compose must consume pack-local tier-3 weights for {kind}"
        );
    }
}
