//! Admission through public dispatch with an isolated, populated knowledge corpus.
use std::collections::HashMap;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

const CHINESE: &str = "如何提升中文检索质量";
const LATIN: &str = "dense sparse retrieval ranking quality";

async fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "knowledge".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
    let registry = builder.build().expect("registry");
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("knowledge schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    let response = registry.dispatch("knowledge.upsert_domains", json!({"domains": [
        {"slug": "cjk-admission", "name": CHINESE, "description": "如何提升中文检索质量 dense sparse retrieval ranking quality domain explains search methods across languages with deterministic corpus fixtures covering matching scoring relevance evaluation and retrieval techniques"},
        {"slug": "latin-admission", "name": LATIN, "description": "dense sparse retrieval ranking quality domain explains search methods across languages with deterministic corpus fixtures covering matching scoring relevance evaluation and retrieval techniques"}
    ]})).await.expect("seed domains");
    assert_eq!(response["created"], 2);
    registry
}

async fn admitted(registry: &VerbRegistry, query: &str) -> Value {
    let response = registry
        .dispatch("knowledge.suggest", json!({"query": query}))
        .await
        .unwrap_or_else(|error| panic!("query {query:?} must be admitted: {error}"));
    assert!(response["results"].is_array(), "{response}");
    response
}

async fn refused(registry: &VerbRegistry, query: &str, count: usize) {
    let error = registry
        .dispatch("knowledge.suggest", json!({"query": query}))
        .await
        .expect_err("short query must be refused before retrieval");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("wrong refusal for {query:?}: {error:?}");
    };
    assert!(
        message.contains("suggest query must be at least 5 words"),
        "{message}"
    );
    assert!(
        message.contains(&format!("got {count};")),
        "{query:?}: {message}"
    );
}

#[tokio::test]
async fn unspaced_chinese_reaches_populated_domain_retrieval() {
    assert_eq!(CHINESE.chars().count(), 10);
    let registry = fixture().await;
    let response = admitted(&registry, CHINESE).await;
    assert!(
        response["results"].as_array().unwrap().iter().any(|row| {
            row["name"] == CHINESE
                && row["id"].is_string()
                && row["score"].as_f64().is_some_and(|score| score > 0.0)
        }),
        "admission must reach the matching domain: {response}"
    );
}

#[tokio::test]
async fn short_cjk_and_punctuation_do_not_satisfy_admission() {
    let registry = fixture().await;
    for (query, count) in [
        ("检索", 1),
        ("如何提升中文检索质", 4),
        ("・・・・・・・・・・", 0),
        ("・ ・ ・ ・ ・", 0),
        ("。！？……", 1),
        ("😀😀😀😀😀", 1),
    ] {
        refused(&registry, query, count).await;
    }
}

#[tokio::test]
async fn latin_whitespace_admission_is_unchanged() {
    let registry = fixture().await;
    refused(&registry, "dense sparse retrieval ranking", 4).await;
    for query in [LATIN, "dense\tsparse\nretrieval\u{2003}ranking quality"] {
        let response = admitted(&registry, query).await;
        assert!(
            response["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["name"] == LATIN),
            "{response}"
        );
    }
    // Admission is a length rule, not a new global content-quality filter.
    admitted(&registry, "1 2 3 4 5").await;
    admitted(&registry, "! ? . , ;").await;
}

#[tokio::test]
async fn mixed_queries_floor_total_cjk_letters_without_double_counting() {
    let registry = fixture().await;
    refused(&registry, "如何提升中文检索质retrieval", 4).await;
    admitted(&registry, "如何提升中文检索质量retrieval").await;
    admitted(&registry, "如何提升中文检索质 retrieval").await;
    admitted(&registry, "如 何 dense sparse retrieval ranking").await;
    refused(&registry, "如 dense sparse retrieval ranking", 4).await;
    refused(&registry, "如・ dense sparse retrieval ranking", 4).await;
}

#[tokio::test]
async fn kana_and_hangul_use_the_same_ten_letter_boundary() {
    let registry = fixture().await;
    for query in [
        "ひらがなかたかなもじ",
        "アイウエオカキクケコ",
        "한국어검색품질개선법",
    ] {
        assert_eq!(query.chars().count(), 10, "{query}");
        admitted(&registry, query).await;
        let nine: String = query.chars().take(9).collect();
        refused(&registry, &nine, 4).await;
    }
}

#[tokio::test]
async fn auto_compose_retains_its_ten_word_threshold() {
    let registry = fixture().await;
    for (query, count) in [
        (CHINESE, 5),
        (
            "dense sparse retrieval ranking quality across languages and domains",
            9,
        ),
    ] {
        let error = registry
            .dispatch("knowledge.compose", json!({"query": query}))
            .await
            .expect_err("auto-compose still requires ten counted words");
        let RuntimeError::InvalidInput(message) = error else {
            panic!("{error:?}")
        };
        assert!(
            message.contains("auto-compose query must be at least 10 words"),
            "{message}"
        );
        assert!(message.contains(&format!("got {count}")), "{message}");
    }
}
