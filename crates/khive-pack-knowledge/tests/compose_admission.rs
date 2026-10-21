use std::collections::HashMap;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

const CHINESE: &str = "如何提升中文检索质量并且评估跨语召回效果";
const LATIN: &str = "dense sparse retrieval ranking quality across languages and knowledge domains";
const MEMBER: &str = "compose-admission-member";
const DOMAIN: &str = "compose-admission-domain";

async fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "knowledge".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
    let registry = builder.build().expect("registry");
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("knowledge schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    let body = format!("{CHINESE} {LATIN} provides deterministic guidance for evaluating lexical and semantic search quality using corpus fixtures with measured relevance and multilingual retrieval examples");
    let atoms = registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": MEMBER, "name": "Populated compose admission member", "content": body
            }]}),
        )
        .await
        .expect("seed member");
    assert_eq!(atoms["created"], 1);
    let domains = registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": DOMAIN, "name": CHINESE, "description": body, "members": [MEMBER]
            }]}),
        )
        .await
        .expect("seed domain");
    assert_eq!(domains["created"], 1);
    registry
}

async fn compose(registry: &VerbRegistry, query: &str) -> Value {
    let response = registry
        .dispatch(
            "knowledge.compose",
            json!({"query": query, "blend_kg": false}),
        )
        .await
        .unwrap_or_else(|error| panic!("query {query:?} must be admitted: {error}"));
    assert_eq!(response["status"], "ok", "{response}");
    assert!(
        response["data"].get("suggest_error").is_none(),
        "{response}"
    );
    response
}

fn assert_member(response: &Value) {
    assert_eq!(response["data"]["count"], 1, "{response}");
    assert_eq!(response["data"]["atoms"][0]["slug"], MEMBER, "{response}");
    assert!(
        response["data"]["markdown"]
            .as_str()
            .unwrap()
            .contains("Populated compose admission member"),
        "{response}"
    );
}

async fn refused(registry: &VerbRegistry, query: &str, count: usize) {
    let error = registry
        .dispatch(
            "knowledge.compose",
            json!({"query": query, "blend_kg": false}),
        )
        .await
        .expect_err("auto-compose must refuse fewer than ten counted words");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("wrong refusal: {error:?}");
    };
    assert!(
        message.contains("auto-compose query must be at least 10 words"),
        "{message}"
    );
    assert!(
        message.contains(&format!("got {count}")),
        "{query:?}: {message}"
    );
}

#[tokio::test]
async fn auto_compose_twenty_cjk_letters_reaches_populated_domain() {
    assert_eq!(CHINESE.chars().count(), 20);
    let registry = fixture().await;
    let response = compose(&registry, CHINESE).await;
    assert_member(&response);
    assert_eq!(response["data"]["domains"][0]["slug"], DOMAIN);
    let nineteen: String = CHINESE.chars().take(19).collect();
    refused(&registry, &nineteen, 9).await;
}

#[tokio::test]
async fn auto_compose_mixed_queries_floor_cjk_letters_once() {
    let registry = fixture().await;
    refused(
        &registry,
        "如何提升中文检索质 dense sparse retrieval ranking quality",
        9,
    )
    .await;
    let response = compose(
        &registry,
        "如何提升中文检索质量 dense sparse retrieval ranking quality",
    )
    .await;
    assert_member(&response);
    let nineteen: String = CHINESE.chars().take(19).collect();
    refused(&registry, &format!("{nineteen}retrieval"), 9).await;
    compose(&registry, &format!("{nineteen} retrieval")).await;
    refused(
        &registry,
        "如 dense sparse retrieval ranking quality across languages knowledge domains",
        9,
    )
    .await;
    compose(
        &registry,
        "如 何 dense sparse retrieval ranking quality across languages knowledge domains",
    )
    .await;
}

#[tokio::test]
async fn auto_compose_keeps_latin_ten_word_threshold_and_suggest_five() {
    assert_eq!(LATIN.split_whitespace().count(), 10);
    let registry = fixture().await;
    let nine = LATIN
        .split_whitespace()
        .take(9)
        .collect::<Vec<_>>()
        .join(" ");
    refused(&registry, &nine, 9).await;
    assert_member(&compose(&registry, LATIN).await);
    assert_member(&compose(&registry, &LATIN.replace(' ', "\t")).await);
    let five = "dense sparse retrieval ranking quality";
    refused(&registry, five, 5).await;
    let suggested = registry
        .dispatch("knowledge.suggest", json!({"query": five}))
        .await
        .unwrap();
    assert!(
        suggested["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == CHINESE),
        "{suggested}"
    );
    let ten_cjk: String = CHINESE.chars().take(10).collect();
    refused(&registry, &ten_cjk, 5).await;
    let suggested = registry
        .dispatch("knowledge.suggest", json!({"query": ten_cjk}))
        .await
        .unwrap();
    assert!(
        suggested["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == CHINESE),
        "{suggested}"
    );
}

#[tokio::test]
async fn explicit_domain_and_atom_ids_still_allow_short_nonempty_queries() {
    let registry = fixture().await;
    let error = registry
        .dispatch(
            "knowledge.compose",
            json!({"query": " ", "blend_kg": false}),
        )
        .await
        .expect_err("auto-compose does not permit an empty query");
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref message) if message == "query must not be empty")
    );
    for (field, slug) in [("domain_ids", DOMAIN), ("atom_ids", MEMBER)] {
        let record = registry
            .dispatch("knowledge.get", json!({"id": slug}))
            .await
            .unwrap();
        let id = record["id"].as_str().unwrap();
        for query in ["检索", "retrieval"] {
            let mut args = json!({"query": query, "blend_kg": false});
            args[field] = json!([id]);
            let response = registry.dispatch("knowledge.compose", args).await.unwrap();
            assert_member(&response);
        }
        let mut args = json!({"query": " ", "blend_kg": false});
        args[field] = json!([id]);
        let error = registry
            .dispatch("knowledge.compose", args)
            .await
            .expect_err("IDs do not permit an empty query");
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message == "query must not be empty")
        );
    }
}
