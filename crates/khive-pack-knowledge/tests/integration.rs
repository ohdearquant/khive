//! Integration tests for the knowledge pack against an in-memory runtime.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

// ── test fixture ──────────────────────────────────────────────────────────────

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

struct Fixture {
    registry: VerbRegistry,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

// ── pack metadata ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn pack_registers_cleanly_with_verb_registry() {
    let f = pack(rt());
    let verbs: Vec<&str> = f.registry.all_verbs().iter().map(|v| v.name).collect();
    assert!(
        verbs.contains(&"knowledge.learn"),
        "expected 'learn' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.cite"),
        "expected 'cite' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.topic"),
        "expected 'topic' verb, got: {verbs:?}"
    );
    // No note kinds added.
    let note_kinds: Vec<&str> = f.registry.all_note_kinds();
    assert!(
        !note_kinds.contains(&"knowledge"),
        "knowledge pack should not add note kinds"
    );
}

// ── learn verb ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn learn_creates_concept_with_name_and_domain() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({
                "name": "LoRA",
                "description": "Low-Rank Adaptation of large language models",
                "domain": "fine-tuning",
                "tags": ["adapter"]
            }),
        )
        .await
        .expect("learn ok");

    assert_eq!(resp["kind"], "concept");
    assert_eq!(resp["name"], "LoRA");
    assert_eq!(resp["domain"], "fine-tuning");
    // Domain is promoted to tags.
    let tags = resp["tags"].as_array().expect("tags array");
    let tag_strs: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        tag_strs.contains(&"fine-tuning"),
        "domain not in tags: {tag_strs:?}"
    );
    assert!(tag_strs.contains(&"adapter"));
    // Response shape: short id (8 chars) + full UUID.
    let id = resp["id"].as_str().expect("id");
    let full_id = resp["full_id"].as_str().expect("full_id");
    assert_eq!(id.len(), 8, "expected 8-char short id, got: {id}");
    assert!(
        full_id.contains('-'),
        "expected UUID in full_id, got: {full_id}"
    );
}

#[tokio::test]
async fn learn_creates_concept_without_domain() {
    let f = pack(rt());
    let resp = f
        .dispatch("knowledge.learn", json!({ "name": "FlashAttention" }))
        .await
        .expect("learn ok");

    assert_eq!(resp["kind"], "concept");
    assert_eq!(resp["name"], "FlashAttention");
    assert!(resp["domain"].is_null());
}

#[tokio::test]
async fn learn_rejects_empty_name() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.learn", json!({ "name": "   " }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("name must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn learn_rejects_missing_name() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.learn", json!({ "domain": "attention" }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    // serde deserialization error: missing field `name`
    assert!(!msg.is_empty(), "expected error for missing name");
}

// ── cite verb ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cite_creates_introduced_by_edge() {
    let f = pack(rt());

    // Create concept via learn.
    let concept = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "LoRA", "domain": "fine-tuning" }),
        )
        .await
        .expect("learn concept");

    // Create paper as a `document` entity (base allowlist: concept -[introduced_by]-> document).
    let paper = f
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "name": "Hu et al. 2021",
                "description": "LoRA: Low-Rank Adaptation paper"
            }),
        )
        .await
        .expect("create document");

    let concept_id = concept["full_id"].as_str().unwrap();
    // `create` serialises the raw Entity — id field is the full UUID string.
    let source_id = paper["id"].as_str().unwrap();

    let resp = f
        .dispatch(
            "knowledge.cite",
            json!({
                "concept_id": concept_id,
                "source_id": source_id,
                "weight": 1.0
            }),
        )
        .await
        .expect("cite ok");

    assert_eq!(resp["relation"], "introduced_by");
    assert_eq!(resp["concept_id"], concept_id);
    assert_eq!(resp["source_id"], source_id);
    assert_eq!(resp["weight"], 1.0);
    let id = resp["id"].as_str().expect("id");
    assert_eq!(id.len(), 8, "expected 8-char edge id, got: {id}");
}

#[tokio::test]
async fn cite_rejects_unknown_id() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.cite",
            json!({
                "concept_id": "00000000-0000-0000-0000-000000000001",
                "source_id":  "00000000-0000-0000-0000-000000000002"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(!msg.is_empty(), "expected error for unknown IDs, got empty");
}

#[tokio::test]
async fn cite_rejects_missing_concept_id() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.cite",
            json!({ "source_id": "00000000-0000-0000-0000-000000000001" }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(!msg.is_empty(), "expected deserialization error");
}

// ── topic verb ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn topic_lists_all_concepts_without_filter() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "GQA", "domain": "attention" }),
    )
    .await
    .expect("learn 1");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "FlashAttention", "domain": "attention" }),
    )
    .await
    .expect("learn 2");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn 3");

    let resp = f
        .dispatch("knowledge.topic", json!({}))
        .await
        .expect("topic ok");

    let items = resp["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3, "expected 3 concepts, got: {}", items.len());
}

#[tokio::test]
async fn topic_filters_by_domain() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "GQA", "domain": "attention" }),
    )
    .await
    .expect("learn 1");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "FlashAttention", "domain": "attention" }),
    )
    .await
    .expect("learn 2");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn 3");

    let resp = f
        .dispatch("knowledge.topic", json!({ "domain": "attention" }))
        .await
        .expect("topic filtered");

    let items = resp["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        2,
        "expected 2 attention concepts, got: {}",
        items.len()
    );

    let names: Vec<&str> = items.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"GQA"), "expected GQA in items: {names:?}");
    assert!(
        names.contains(&"FlashAttention"),
        "expected FlashAttention: {names:?}"
    );
}

#[tokio::test]
async fn topic_returns_empty_for_unknown_domain() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn");

    let resp = f
        .dispatch("knowledge.topic", json!({ "domain": "quantum-computing" }))
        .await
        .expect("topic ok");

    let items = resp["items"].as_array().expect("items array");
    assert!(items.is_empty(), "expected 0 items for unknown domain");
}

#[tokio::test]
async fn topic_respects_limit() {
    let f = pack(rt());
    for i in 0..5 {
        f.dispatch("knowledge.learn", json!({ "name": format!("Concept{i}") }))
            .await
            .expect("learn");
    }

    let resp = f
        .dispatch("knowledge.topic", json!({ "limit": 2 }))
        .await
        .expect("topic ok");

    let items = resp["items"].as_array().expect("items array");
    assert!(items.len() <= 2, "expected ≤ 2 items, got: {}", items.len());
}

// ── H1 regression: case-insensitive domain filter (ADR-047 §91) ──────────────

#[tokio::test]
async fn topic_domain_filter_is_case_insensitive_listing_path() {
    let f = pack(rt());

    // Store concept with uppercase domain "Attention".
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "FlashAttention", "domain": "Attention" }),
    )
    .await
    .expect("learn with Attention");

    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn with fine-tuning");

    // Query with lowercase "attention" — must find the concept stored as "Attention".
    let resp = f
        .dispatch("knowledge.topic", json!({ "domain": "attention" }))
        .await
        .expect("topic ok");

    let items = resp["items"].as_array().expect("items array");
    let names: Vec<&str> = items.iter().filter_map(|v| v["name"].as_str()).collect();
    assert_eq!(items.len(), 1, "expected 1 match, got: {names:?}");
    assert!(
        names.contains(&"FlashAttention"),
        "expected FlashAttention in results: {names:?}"
    );
    assert_eq!(
        resp["total"].as_u64().unwrap_or(0),
        1,
        "total should be 1 on listing path"
    );
}

// ── H2 regression: search-path `total` semantics ─────────────────────────────

#[tokio::test]
async fn topic_search_path_total_is_bounded_by_candidate_window() {
    let f = pack(rt());

    // Learn 10 concepts — more than a small limit, so we can observe truncation.
    for i in 0..10 {
        f.dispatch(
            "knowledge.learn",
            json!({ "name": format!("Attention{i}"), "domain": "attention" }),
        )
        .await
        .expect("learn");
    }
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn unrelated");

    // Search path with limit=3.  total must be <= limit*4 (12) and >= returned items.
    let resp = f
        .dispatch(
            "knowledge.topic",
            json!({ "query": "attention", "limit": 3 }),
        )
        .await
        .expect("topic search ok");

    let items = resp["items"].as_array().expect("items array");
    let total = resp["total"].as_u64().expect("total field present");

    assert!(
        items.len() <= 3,
        "items must respect limit: got {}",
        items.len()
    );
    // total is the candidate-window count, bounded by limit*4 = 12.
    assert!(
        total <= 12,
        "search-path total must be bounded by limit*4 (12), got {total}"
    );
    assert!(
        total >= items.len() as u64,
        "total must be >= returned items: total={total}, items={}",
        items.len()
    );
}
