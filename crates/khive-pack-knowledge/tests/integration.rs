// FILE SIZE JUSTIFICATION: This file covers the full public verb surface of the knowledge
// pack (upsert_atoms, upsert_domains, get, list, delete_atoms, stats, index, fold, search,
// suggest, compose, edit, import, challenge, adjudicate, learn, cite, topic) with multiple
// scenarios per verb (happy path, edge cases, namespace isolation, pagination). Each test
// requires a fresh in-memory runtime, making per-verb test file splitting impractical without
// re-creating the same setup boilerplate in every file. Splitting is deferred until shared
// test fixtures can be extracted into a crate-level test helper module.

//! Integration tests for the knowledge pack against an in-memory runtime.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, PackRegistry, RequestIdentity,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// ── test fixture ──────────────────────────────────────────────────────────────

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

struct Fixture {
    registry: VerbRegistry,
}

#[derive(Debug, Default)]
struct NestedProfileIdentityGate {
    requests: Mutex<Vec<(String, String, String)>>,
}

impl Gate for NestedProfileIdentityGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        self.requests.lock().expect("gate request lock").push((
            req.verb.clone(),
            req.actor.id.clone(),
            req.namespace.as_str().to_string(),
        ));
        if matches!(req.verb.as_str(), "brain.resolve" | "brain.profile")
            && req.actor.id != "requester"
        {
            return Ok(GateDecision::deny(
                "nested profile reads require the per-request principal",
            ));
        }
        Ok(GateDecision::allow())
    }
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
    registry.apply_schema_plans(rt.backend());
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
    // Corpus-tier verbs must also be registered.
    assert!(
        verbs.contains(&"knowledge.upsert_atoms"),
        "expected 'knowledge.upsert_atoms' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.search"),
        "expected 'knowledge.search' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.fold"),
        "expected 'knowledge.fold' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.suggest"),
        "expected 'knowledge.suggest' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.compose"),
        "expected 'knowledge.compose' verb, got: {verbs:?}"
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
                "description": "Low-Rank Adaptation of large language models — covering concepts techniques algorithms implementations applications use cases and design patterns in detail",
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
async fn learn_rejects_missing_name_and_content() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.learn", json!({ "domain": "attention" }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    // Neither name nor content supplied — handler returns a descriptive error.
    assert!(
        msg.contains("name must not be empty"),
        "expected descriptive error, got: {msg}"
    );
}

// ── learn content-alias (issue #488) ─────────────────────────────────────────

#[tokio::test]
async fn learn_content_without_name_auto_generates_name() {
    let f = pack(rt());
    // Agent-style call: only `content` provided, no explicit `name`.
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({ "content": "Some long description about X that keeps going and going beyond sixty characters easily dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }),
        )
        .await
        .expect("learn with content only should succeed");

    assert_eq!(resp["kind"], "concept");
    let name = resp["name"].as_str().expect("name present");
    assert!(!name.is_empty(), "auto-generated name must not be empty");
    assert!(
        name.len() <= 60,
        "auto-generated name must be <= 60 chars, got: {name:?}"
    );
    // Description is populated from `content`.
    let desc = resp["description"].as_str().expect("description present");
    assert!(
        desc.contains("Some long description"),
        "description should contain content: {desc:?}"
    );
}

#[tokio::test]
async fn learn_content_alias_maps_to_description() {
    let f = pack(rt());
    // When both `name` and `content` are provided, content becomes the description.
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({
                "name": "GQA",
                "content": "Grouped-Query Attention mechanism"
            }),
        )
        .await
        .expect("learn with name + content");

    assert_eq!(resp["name"], "GQA");
    assert_eq!(resp["description"], "Grouped-Query Attention mechanism");
}

#[tokio::test]
async fn learn_short_content_uses_full_text_as_name() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({ "content": "Speculative Decoding" }),
        )
        .await
        .expect("learn short content");

    assert_eq!(resp["name"], "Speculative Decoding");
    assert_eq!(resp["description"], "Speculative Decoding");
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
                "description": "LoRA: Low-Rank Adaptation paper — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering"
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
async fn cite_accepts_org_source() {
    let f = pack(rt());

    let concept = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "Hopper Architecture", "domain": "hardware" }),
        )
        .await
        .expect("learn concept");

    // Base allowlist (ADR-002 2026-07-08 amendment): concept -[introduced_by]-> org.
    let org = f
        .dispatch(
            "create",
            json!({
                "kind": "org",
                "name": "Example Hardware Vendor",
                "description": "Org entity originating an architecture concept — covering concepts techniques algorithms implementations applications use cases and design patterns in detail"
            }),
        )
        .await
        .expect("create org");

    let concept_id = concept["full_id"].as_str().unwrap();
    let source_id = org["id"].as_str().unwrap();

    let resp = f
        .dispatch(
            "knowledge.cite",
            json!({ "concept_id": concept_id, "source_id": source_id }),
        )
        .await
        .expect("cite org source ok");

    assert_eq!(resp["relation"], "introduced_by");
    assert_eq!(resp["source_id"], source_id);
}

/// Regression (PR #1623 round 3): the vocab promises unique 8+ hex prefix
/// resolution for `concept_id` and `source_id`. Both must resolve through
/// `resolve_uuid`'s prefix arm and return canonical full UUIDs.
#[tokio::test]
async fn cite_resolves_concept_and_source_by_unique_prefix() {
    let f = pack(rt());

    let concept = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "QuaRot", "domain": "quantization" }),
        )
        .await
        .expect("learn concept");
    let paper = f
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "name": "Ashkboos et al. 2024",
                "description": "QuaRot paper — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering"
            }),
        )
        .await
        .expect("create document");

    let concept_full = concept["full_id"].as_str().unwrap();
    let source_full = paper["id"].as_str().unwrap();
    let concept_prefix = &concept_full[..8];
    let source_prefix = &source_full[..8];

    let resp = f
        .dispatch(
            "knowledge.cite",
            json!({
                "concept_id": concept_prefix,
                "source_id": source_prefix,
                "weight": 0.7
            }),
        )
        .await
        .expect("prefixes must resolve on both cite parameters");

    assert_eq!(resp["concept_id"], concept_full);
    assert_eq!(resp["source_id"], source_full);
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

    let items = resp["results"].as_array().expect("results array");
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

    let items = resp["results"].as_array().expect("results array");
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

    let items = resp["results"].as_array().expect("results array");
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

    let items = resp["results"].as_array().expect("results array");
    assert!(
        items.len() <= 2,
        "expected <= 2 items, got: {}",
        items.len()
    );
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

    let items = resp["results"].as_array().expect("results array");
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

    let items = resp["results"].as_array().expect("results array");
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

// ── upsert_atoms ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn upsert_atoms_creates_new_atoms() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "rag", "name": "RAG", "content": "RAG retrieves relevant passages before generating. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "tags": ["retrieval", "rag"] },
                    { "slug": "lora", "name": "LoRA", "content": "Low-Rank Adaptation of LLMs — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering", "tags": ["fine-tuning", "adapter"] },
                    { "slug": "flash-attention", "name": "FlashAttention", "content": "Memory-efficient attention — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques", "tags": ["attention"] },
                ]
            }),
        )
        .await
        .expect("upsert_atoms ok");

    assert_eq!(resp["created"], 3, "expected 3 created");
    assert_eq!(resp["updated"], 0, "expected 0 updated");
    assert_eq!(resp["total"], 3);
}

#[tokio::test]
async fn upsert_atoms_updates_on_second_call() {
    let f = pack(rt());
    // First insert.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "rag", "name": "RAG", "content": "original content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("first upsert");

    // Second call with same slug — should update.
    let resp = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": "rag", "name": "RAG updated", "content": "updated content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .expect("second upsert");

    assert_eq!(resp["created"], 0, "expected 0 created on second call");
    assert_eq!(resp["updated"], 1, "expected 1 updated");

    // Verify get returns the updated name.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "rag" }))
        .await
        .expect("get ok");
    assert_eq!(got["name"], "RAG updated");
    assert_eq!(got["slug"], "rag");
}

#[tokio::test]
async fn upsert_atoms_rejects_empty_list() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.upsert_atoms", json!({ "atoms": [] }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"), "got: {err}");
}

#[tokio::test]
async fn upsert_atoms_rejects_empty_slug() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": "  ", "name": "Bad", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("slug"), "got: {err}");
}

#[tokio::test]
async fn upsert_atoms_rejects_reserved_secret_gate_property_key() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": "reserved-key-atom",
                "name": "Reserved key atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                "properties": { "khive:secret_gate": "exempted:content-sha256-manifest-v1" }
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("khive:secret_gate") && err.to_string().contains("runtime-owned"),
        "got: {err}"
    );

    let list = f
        .dispatch("knowledge.list", json!({}))
        .await
        .expect("list ok");
    assert_eq!(
        list["atoms"].as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "rejected atom must not be persisted"
    );
}

// ── upsert_domains ────────────────────────────────────────────────────────────

#[tokio::test]
async fn upsert_domains_creates_and_updates() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    { "slug": "retrieval", "name": "Retrieval", "description": "Retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques", "members": ["rag", "dense-retrieval"] }
                ]
            }),
        )
        .await
        .expect("upsert_domains ok");

    assert_eq!(resp["created"], 1);
    assert_eq!(resp["updated"], 0);

    // Second call — update.
    let resp2 = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    { "slug": "retrieval", "name": "Retrieval updated", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "members": ["rag", "dense-retrieval", "bm25"] }
                ]
            }),
        )
        .await
        .expect("second upsert_domains ok");

    assert_eq!(resp2["created"], 0);
    assert_eq!(resp2["updated"], 1);

    // get by slug returns updated name.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "retrieval" }))
        .await
        .expect("get domain ok");
    assert_eq!(got["name"], "Retrieval updated");
    assert_eq!(got["kind"], "domain");
    let members = got["members"].as_array().expect("members array");
    assert_eq!(members.len(), 3);
}

#[tokio::test]
async fn upsert_domains_rejects_empty_list() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.upsert_domains", json!({ "domains": [] }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"), "got: {err}");
}

#[tokio::test]
async fn upsert_domains_rejects_atom_slug_collision_without_partial_domain() {
    let f = pack(rt());

    // Seed a normal atom that owns the slug the domain upsert will collide on.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "shared-slug",
            "name": "Original Atom Name",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "tags": ["distinctive-tag"],
        }] }),
    )
    .await
    .expect("seed atom");

    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{
                "slug": "shared-slug",
                "name": "Colliding Domain",
                "description": "covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques",
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // No domain row must exist after the rejected upsert (no partial commit).
    let domains = f
        .dispatch("knowledge.list", json!({ "type": "domain" }))
        .await
        .expect("list domains ok");
    let results = domains["results"].as_array().expect("results array");
    assert!(
        results
            .iter()
            .all(|d| d["slug"].as_str() != Some("shared-slug")),
        "no domain with slug 'shared-slug' should exist after rejected collision: {results:?}"
    );

    // Retry — still rejected, and the original atom is untouched.
    let err2 = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{
                "slug": "shared-slug",
                "name": "Colliding Domain Retry",
                "description": "covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques",
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err2, RuntimeError::InvalidInput(_)),
        "expected InvalidInput on retry, got: {err2:?}"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "shared-slug" }))
        .await
        .expect("original atom still resolvable");
    assert_eq!(atom["kind"], "atom");
    assert_eq!(atom["name"], "Original Atom Name");
    assert_eq!(
        atom["content"],
        "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
    );
    let tags = atom["tags"].as_array().expect("tags array");
    assert!(tags.iter().any(|t| t == "distinctive-tag"));
}

#[tokio::test]
async fn upsert_atoms_rejects_domain_mirror_slug_collision_without_mutation() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "retrieval",
            "name": "Retrieval",
            "description": "Dense and sparse retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —",
        }] }),
    )
    .await
    .expect("upsert domain");

    // A plain upsert_atoms call targeting the domain's mirror slug must be
    // rejected, not silently strip type:domain from the mirror's tags.
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": "retrieval",
                "name": "Retrieval Atom Overwrite Attempt",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // The domain mirror must be untouched: still classified as a domain by
    // direct lookup and by search, with its original name intact.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "retrieval" }))
        .await
        .expect("get must still resolve the domain");
    assert_eq!(got["kind"], "domain");
    assert_eq!(got["name"], "Retrieval");

    let search = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["slug"] == "retrieval"),
        "search must still find the domain, not a demoted atom: {results:?}"
    );
}

// ── knowledge.get ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_atom_by_slug() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "lora", "name": "LoRA", "content": "Low-Rank Adaptation — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("upsert");

    let got = f
        .dispatch("knowledge.get", json!({ "id": "lora" }))
        .await
        .expect("get ok");

    assert_eq!(got["slug"], "lora");
    assert_eq!(got["name"], "LoRA");
    assert_eq!(got["kind"], "atom");
}

#[tokio::test]
async fn get_returns_not_found_for_unknown_slug() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.get", json!({ "id": "nonexistent-slug-xyz" }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("NotFound"),
        "expected not-found error, got: {err}"
    );
}

#[tokio::test]
async fn get_by_domain_uuid_returns_canonical_domain_not_mirror_atom() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "uuid-domain", "name": "UUID Domain", "description": "Retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques", "members": ["rag"] }
            ]
        }),
    )
    .await
    .expect("upsert_domains ok");

    // Resolve the domain's UUID via the slug path (already correct).
    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "uuid-domain" }))
        .await
        .expect("get by slug ok");
    assert_eq!(by_slug["kind"], "domain");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // The UUID path must agree with the slug path: canonical domain, not the mirror atom.
    let by_uuid = f
        .dispatch("knowledge.get", json!({ "id": uuid }))
        .await
        .expect("get by uuid ok");
    assert_eq!(
        by_uuid["kind"], "domain",
        "UUID lookup must return the canonical domain, got: {by_uuid}"
    );
    let members = by_uuid["members"]
        .as_array()
        .expect("members must be present and an array");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0], "rag");
}

#[tokio::test]
async fn get_resolves_atom_by_compact_prefix_longer_than_eight_chars() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "prefix-atom",
            "name": "Prefix Atom",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "prefix-atom" }))
        .await
        .expect("get atom by slug");
    let full_id = by_slug["id"].as_str().expect("full atom id");
    let compact: String = full_id.chars().filter(|ch| *ch != '-').take(12).collect();

    let by_prefix = f
        .dispatch("knowledge.get", json!({ "id": compact }))
        .await
        .expect("get atom by unique compact prefix");
    assert_eq!(by_prefix["id"], full_id);
    assert_eq!(by_prefix["kind"], "atom");

    let compact_uuid = full_id.replace('-', "");
    let by_compact_uuid = f
        .dispatch("knowledge.get", json!({ "id": compact_uuid }))
        .await
        .expect("32-character compact UUID is a complete identifier");
    assert_eq!(by_compact_uuid["id"], full_id);
}

#[tokio::test]
async fn get_exact_all_hex_slug_wins_over_uuid_prefix_collision() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "hex-prefix-source",
            "name": "Hex Prefix Source",
            "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
        }] }),
    )
    .await
    .expect("upsert prefix source");

    let source = f
        .dispatch("knowledge.get", json!({ "id": "hex-prefix-source" }))
        .await
        .expect("get prefix source");
    let source_id = source["id"].as_str().expect("source id").to_string();
    let hex_slug = source_id.replace('-', "")[..16].to_string();

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": hex_slug.clone(),
            "name": "Exact Hex Slug",
            "content": "exact hexadecimal slug retrieval must precede compact identifier prefix interpretation across knowledge corpus reads and preserve deterministic registered slug addressing"
        }] }),
    )
    .await
    .expect("upsert exact all-hex slug");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": hex_slug.clone() }))
        .await
        .expect("exact all-hex slug must win over UUID prefix collision");
    assert_eq!(by_slug["slug"], hex_slug);
    assert_eq!(by_slug["name"], "Exact Hex Slug");
    assert_ne!(
        by_slug["id"], source_id,
        "prefix interpretation must not return the colliding source record"
    );
}

#[tokio::test]
async fn get_exact_all_hex_slug_wins_when_uuid_prefix_matches_nothing() {
    const HEX_SLUG: &str = "fffffffffffffffffffffffffffffffff";
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": HEX_SLUG,
            "name": "Overlong Hex Slug",
            "content": "overlong hexadecimal slug lookup remains exact and addressable even though no canonical UUID can match a prefix longer than thirty two hexadecimal characters"
        }] }),
    )
    .await
    .expect("upsert overlong all-hex slug");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": HEX_SLUG }))
        .await
        .expect("exact all-hex slug must resolve before a guaranteed prefix miss");
    assert_eq!(by_slug["slug"], HEX_SLUG);
    assert_eq!(by_slug["name"], "Overlong Hex Slug");
}

#[tokio::test]
async fn get_domain_prefix_deduplicates_same_uuid_mirror_atom() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "prefix-domain",
            "name": "Prefix Domain",
            "description": "Retrieval concepts techniques algorithms implementations applications use cases and design patterns in sufficient detail for this deterministic domain prefix regression fixture"
        }] }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "prefix-domain" }))
        .await
        .expect("get domain by slug");
    let full_id = by_slug["id"].as_str().expect("full domain id");
    let prefix = &full_id[..8];

    let by_prefix = f
        .dispatch("knowledge.get", json!({ "id": prefix }))
        .await
        .expect("domain and mirror atom must count as one prefix match");
    assert_eq!(by_prefix["id"], full_id);
    assert_eq!(by_prefix["kind"], "domain");
}

#[tokio::test]
async fn get_rejects_ambiguous_prefix_across_distinct_knowledge_records() {
    let runtime = rt();
    let f = pack(runtime.clone());
    let mut writer = runtime.sql().writer().await.expect("knowledge writer");
    writer
        .execute_batch(vec![
            SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, created_at, updated_at) \
                      VALUES (?1, 'local', 'ambiguous-a', 'Ambiguous A', 'content a', 1, 1)"
                    .into(),
                params: vec![SqlValue::Text(
                    "deadbeef-0000-4000-8000-000000000001".into(),
                )],
                label: Some("test.knowledge_get.ambiguous_a".into()),
            },
            SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, created_at, updated_at) \
                      VALUES (?1, 'local', 'ambiguous-b', 'Ambiguous B', 'content b', 2, 2)"
                    .into(),
                params: vec![SqlValue::Text(
                    "deadbeef-0000-4000-8000-000000000002".into(),
                )],
                label: Some("test.knowledge_get.ambiguous_b".into()),
            },
        ])
        .await
        .expect("seed colliding prefixes");
    drop(writer);

    let err = f
        .dispatch("knowledge.get", json!({ "id": "deadbeef" }))
        .await
        .expect_err("distinct UUIDs sharing a prefix must be ambiguous");
    let RuntimeError::AmbiguousPrefix { prefix, matches } = err else {
        panic!("expected AmbiguousPrefix, got {err:?}");
    };
    assert_eq!(prefix, "deadbeef");
    assert_eq!(matches.len(), 2);
}

#[tokio::test]
async fn get_by_id_is_namespace_agnostic_and_loads_sections_from_stored_namespace() {
    let f = pack(rt());
    let foreign_namespace = "identifier-contract-foreign";
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "namespace": foreign_namespace,
            "atoms": [{
                "slug": "foreign-prefix-atom",
                "name": "Foreign Prefix Atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert foreign atom");
    f.dispatch(
        "knowledge.edit",
        json!({
            "namespace": foreign_namespace,
            "id": "foreign-prefix-atom",
            "sections": [{
                "section_type": "overview",
                "content": "This section belongs to the foreign namespace atom and proves that a namespace-agnostic by-ID read loads sections using the resolved record namespace."
            }]
        }),
    )
    .await
    .expect("add foreign section");

    let foreign = f
        .dispatch(
            "knowledge.get",
            json!({ "namespace": foreign_namespace, "id": "foreign-prefix-atom" }),
        )
        .await
        .expect("get foreign atom by scoped slug");
    let full_id = foreign["id"].as_str().expect("foreign atom id").to_string();
    let compact_prefix = full_id.replace('-', "")[..12].to_string();

    f.dispatch("knowledge.get", json!({ "id": "foreign-prefix-atom" }))
        .await
        .expect_err("slug lookup must remain scoped to the caller namespace");

    let by_id = f
        .dispatch(
            "knowledge.get",
            json!({ "id": full_id.clone(), "include_sections": true }),
        )
        .await
        .expect("full UUID read must be namespace-agnostic");
    assert_eq!(by_id["namespace"], foreign_namespace);
    assert_eq!(
        by_id["sections"].as_array().expect("sections array").len(),
        1,
        "section lookup must use the resolved atom's stored namespace"
    );

    let by_prefix = f
        .dispatch(
            "knowledge.get",
            json!({ "id": compact_prefix, "include_sections": true }),
        )
        .await
        .expect("unique prefix read must be namespace-agnostic");
    assert_eq!(by_prefix["id"], full_id);
    assert_eq!(by_prefix["namespace"], foreign_namespace);
    assert_eq!(
        by_prefix["sections"]
            .as_array()
            .expect("prefix sections array")
            .len(),
        1,
        "prefix section lookup must use the resolved atom's stored namespace"
    );
}

// ── knowledge.get + include_sections ─────────────────────────────────────────

#[tokio::test]
async fn get_include_sections_false_returns_no_sections_key() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "s-atom", "name": "SAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "s-atom", "sections": [{ "section_type": "overview", "content": "This section describes the overview of LoRA and its applications in fine-tuning large language models with low-rank matrix decompositions." }] }),
    )
    .await
    .expect("edit");

    let got = f
        .dispatch("knowledge.get", json!({ "id": "s-atom" }))
        .await
        .expect("get without sections");

    assert_eq!(got["kind"], "atom");
    assert!(
        got.get("sections").is_none(),
        "sections key must not be present by default"
    );
}

#[tokio::test]
async fn get_include_sections_returns_all_sections_ordered() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "sec-atom", "name": "SecAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "sec-atom",
            "sections": [
                { "section_type": "overview", "content": "This is the overview section covering the main ideas and introduction to the topic in sufficient detail for embedding purposes." },
                { "section_type": "formalism", "content": "Formal definitions go here including mathematical notation theorems proofs lemmas and corollaries that describe the system formally." },
                { "section_type": "examples", "content": "Concrete examples illustrate the concepts with worked-through scenarios code samples and practical demonstrations of usage patterns." },
            ]
        }),
    )
    .await
    .expect("edit");

    let got = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "sec-atom", "include_sections": true }),
        )
        .await
        .expect("get with sections");

    assert_eq!(got["kind"], "atom");
    let sections = got["sections"].as_array().expect("sections is array");
    assert_eq!(sections.len(), 3, "expected 3 sections, got: {sections:?}");

    let types: Vec<&str> = sections
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    assert!(types.contains(&"overview"), "missing overview: {types:?}");
    assert!(types.contains(&"formalism"), "missing formalism: {types:?}");
    assert!(types.contains(&"examples"), "missing examples: {types:?}");

    for s in sections {
        assert!(
            s["content"].as_str().is_some_and(|c| !c.is_empty()),
            "section content empty"
        );
        assert!(s["section_type"].as_str().is_some(), "section_type missing");
        assert!(s["sort_order"].as_i64().is_some(), "sort_order missing");
    }
}

#[tokio::test]
async fn get_include_sections_by_uuid() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "uuid-sec-atom", "name": "UuidSecAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "uuid-sec-atom", "sections": [{ "section_type": "overview", "content": "This section describes the overview of LoRA and its applications in fine-tuning large language models with low-rank matrix decompositions." }] }),
    )
    .await
    .expect("edit");

    let by_slug = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "uuid-sec-atom", "include_sections": true }),
        )
        .await
        .expect("get by slug");
    let atom_uuid = by_slug["id"].as_str().expect("id").to_owned();

    let by_uuid = f
        .dispatch(
            "knowledge.get",
            json!({ "id": atom_uuid, "include_sections": true }),
        )
        .await
        .expect("get by uuid");

    let sections = by_uuid["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 1, "expected 1 section by UUID lookup");
}

#[tokio::test]
async fn get_include_sections_namespace_isolation() {
    let f = pack(rt());

    // ADR-007 Rev 2: all storage routes to local. Two DISTINCT slugs each get
    // their own sections; sections from one atom must not leak to the other.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "iso-atom-a", "name": "NSA", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert atom-a");

    f.dispatch(
        "knowledge.edit",
        json!({ "id": "iso-atom-a", "sections": [{ "section_type": "overview", "content": "This section belongs exclusively to atom A and must not be visible when fetching atom B under any circumstances." }] }),
    )
    .await
    .expect("edit atom-a");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "iso-atom-b", "name": "NSB", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert atom-b");

    let got_b = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "iso-atom-b", "include_sections": true }),
        )
        .await
        .expect("get atom-b");

    let sections_b = got_b["sections"].as_array().expect("sections array");
    assert_eq!(sections_b.len(), 0, "atom-b must not see atom-a sections");
}

// Regression: two sections sharing the same sort_order must come back in a
// stable, deterministic order (id ASC as final tie-breaker).
#[tokio::test]
async fn get_include_sections_ordering_tie_break_is_stable() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "tie-atom", "name": "TieAtom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    // Insert two sections with the same sort_order (both default to their
    // SectionType ordinal; explicitly override to the same value to guarantee
    // the tie). Each has distinct content so both rows are inserted.
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "tie-atom",
            "sections": [
                {
                    "section_type": "overview",
                    "content": "First section content for the tie-break test covering overview of the main topic in sufficient detail for the minimum content length requirement to be satisfied.",
                    "sort_order": 5
                },
                {
                    "section_type": "formalism",
                    "content": "Second section content for the tie-break test covering formal definitions and mathematical notation in sufficient detail for the minimum content length requirement.",
                    "sort_order": 5
                },
            ]
        }),
    )
    .await
    .expect("edit");

    // Fetch twice; both calls must return the same order.
    let first = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "tie-atom", "include_sections": true }),
        )
        .await
        .expect("get first");
    let second = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "tie-atom", "include_sections": true }),
        )
        .await
        .expect("get second");

    let s1 = first["sections"].as_array().expect("sections first");
    let s2 = second["sections"].as_array().expect("sections second");

    assert_eq!(s1.len(), 2, "expected 2 sections on first fetch");
    assert_eq!(s2.len(), 2, "expected 2 sections on second fetch");

    // Both rows share sort_order=5; the id-ASC tie-breaker must produce the
    // same sequence across repeated queries.
    let ids_first: Vec<&str> = s1.iter().filter_map(|s| s["id"].as_str()).collect();
    let ids_second: Vec<&str> = s2.iter().filter_map(|s| s["id"].as_str()).collect();
    assert_eq!(
        ids_first, ids_second,
        "section order must be deterministic across repeated fetches (id ASC tie-breaker)"
    );

    // Pin the full ordering contract (sort_order ASC, created_at ASC, id ASC):
    // repeated-read agreement alone can pass on SQLite insertion-order luck
    // even if the id tie-breaker is removed.
    let actual: Vec<(i64, i64, &str)> = s1
        .iter()
        .map(|s| {
            (
                s["sort_order"].as_i64().expect("sort_order"),
                s["created_at"].as_i64().expect("created_at"),
                s["id"].as_str().expect("id"),
            )
        })
        .collect();
    let mut expected = actual.clone();
    expected.sort();
    assert_eq!(
        actual, expected,
        "sections must be sorted by (sort_order, created_at, id)"
    );

    // Both calls must agree on which section type comes first (also validates
    // that the order is NOT random).
    let types_first: Vec<&str> = s1
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    let types_second: Vec<&str> = s2
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    assert_eq!(types_first, types_second, "section_type order must match");
}

// ── knowledge.list ────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_atoms_returns_all_atoms() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "a1", "name": "Alpha", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "a2", "name": "Beta", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "a3", "name": "Gamma", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("upsert");

    let resp = f
        .dispatch("knowledge.list", json!({ "type": "atom" }))
        .await
        .expect("list ok");

    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    assert_eq!(resp["total"], 3);
}

#[tokio::test]
async fn list_domains_returns_only_domains() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "a1", "name": "Alpha", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert atom");
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "d1", "name": "Domain1", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch("knowledge.list", json!({ "type": "domain" }))
        .await
        .expect("list domains ok");

    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["kind"], "domain");
}

#[tokio::test]
async fn list_respects_limit_and_offset() {
    let f = pack(rt());
    for i in 0..10 {
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": format!("a{i}"), "name": format!("Atom{i}"), "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .expect("upsert");
    }

    let page1 = f
        .dispatch("knowledge.list", json!({ "limit": 3, "offset": 0 }))
        .await
        .expect("page1 ok");
    let page2 = f
        .dispatch("knowledge.list", json!({ "limit": 3, "offset": 3 }))
        .await
        .expect("page2 ok");

    let r1 = page1["results"].as_array().expect("r1");
    let r2 = page2["results"].as_array().expect("r2");
    assert_eq!(r1.len(), 3, "page1 should have 3 items");
    assert_eq!(r2.len(), 3, "page2 should have 3 items");
    assert_eq!(page1["total"], 10);
    // IDs on page1 and page2 should not overlap.
    let ids1: std::collections::HashSet<&str> =
        r1.iter().filter_map(|v| v["id"].as_str()).collect();
    let ids2: std::collections::HashSet<&str> =
        r2.iter().filter_map(|v| v["id"].as_str()).collect();
    assert!(
        ids1.is_disjoint(&ids2),
        "page1 and page2 ids must not overlap"
    );
}

/// #1671: a full offset sweep over `knowledge.list` must enumerate every atom
/// exactly once — no duplicates, no misses across page boundaries — even when
/// all atoms share one `created_at` value (the column the primary sort key
/// uses), which this test forces via a direct SQL update.
///
/// The sweep also asserts the concatenated pages follow the documented
/// `created_at DESC, id DESC` order: with one shared `created_at` that is
/// `id DESC`, and uniqueness alone would still pass with a wrong tiebreak
/// direction.
#[tokio::test]
async fn list_offset_sweep_covers_all_atoms_exactly_once() {
    let rt = rt();
    let f = pack(rt.clone());
    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    for chunk in 0..7 {
        let atoms: Vec<Value> = (0..17)
            .map(|i| {
                let n = chunk * 17 + i;
                json!({ "slug": format!("sweep-{n:03}"), "name": format!("Sweep {n}"), "content": content })
            })
            .collect();
        f.dispatch("knowledge.upsert_atoms", json!({ "atoms": atoms }))
            .await
            .expect("upsert");
    }

    // Force every atom onto one shared `created_at` so the sweep exercises the
    // tiebreak path; otherwise unique microsecond timestamps would let the
    // test pass even without the `id` tiebreak.
    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer must open");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: None,
            })
            .await
            .expect("force shared created_at");
    }

    let mut seen = std::collections::HashSet::new();
    let mut ordered_ids: Vec<String> = Vec::new();
    let mut offset = 0_u64;
    let page_size = 13_u64;
    loop {
        let page = f
            .dispatch(
                "knowledge.list",
                json!({ "limit": page_size, "offset": offset }),
            )
            .await
            .expect("list page");
        let results = page["results"].as_array().expect("results array");
        if results.is_empty() {
            break;
        }
        for row in results {
            let id = row["id"].as_str().expect("atom id").to_string();
            assert!(seen.insert(id.clone()), "duplicate id across pages: {id}");
            ordered_ids.push(id);
        }
        offset += results.len() as u64;
    }
    assert_eq!(
        seen.len(),
        7 * 17,
        "sweep must cover every atom exactly once"
    );
    // With one shared `created_at` the documented `created_at DESC, id DESC`
    // order is exactly `id DESC`, so the pages concatenated must be the id
    // list sorted descending.
    let mut expected_order: Vec<String> = seen.iter().cloned().collect();
    expected_order.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        ordered_ids, expected_order,
        "sweep pages must appear in created_at DESC, id DESC order"
    );
}

/// #1671: `knowledge.list(kind="domain")` pages over
/// `created_at DESC, id DESC`. Seed domains, force one shared `created_at`
/// via direct SQL so the `id` tiebreak is load-bearing, then sweep with a
/// small limit and assert no duplicates, no misses, AND the exact
/// `id DESC` order (one shared timestamp reduces the documented order to it).
#[tokio::test]
async fn list_domains_offset_sweep_covers_equal_created_at_in_order() {
    let rt = rt();
    let f = pack(rt.clone());
    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    for n in 0..37 {
        f.dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{ "slug": format!("dom-{n:03}"), "name": format!("Domain {n}"), "description": content }] }),
        )
        .await
        .expect("upsert domain");
    }

    // Force every domain onto one shared `created_at` so the sweep exercises
    // the tiebreak path; otherwise unique microsecond timestamps would let
    // the test pass even without the `id` tiebreak.
    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer must open");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_domains SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: None,
            })
            .await
            .expect("force shared created_at");
    }

    let mut seen = std::collections::HashSet::new();
    let mut ordered_ids: Vec<String> = Vec::new();
    let mut offset = 0_u64;
    let page_size = 9_u64;
    loop {
        let page = f
            .dispatch(
                "knowledge.list",
                json!({ "kind": "domain", "limit": page_size, "offset": offset }),
            )
            .await
            .expect("list domains page");
        let results = page["results"].as_array().expect("results array");
        if results.is_empty() {
            break;
        }
        for row in results {
            let id = row["id"].as_str().expect("domain id").to_string();
            assert!(seen.insert(id.clone()), "duplicate id across pages: {id}");
            ordered_ids.push(id);
        }
        offset += results.len() as u64;
    }
    assert_eq!(
        seen.len(),
        37,
        "domain sweep must cover every domain exactly once"
    );
    // One shared `created_at` reduces the documented `created_at DESC,
    // id DESC` order to `id DESC`, so the pages concatenated must be the id
    // list sorted descending.
    let mut expected_order: Vec<String> = seen.iter().cloned().collect();
    expected_order.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        ordered_ids, expected_order,
        "domain sweep pages must appear in created_at DESC, id DESC order"
    );
}

/// #1671: the batch re-embed sweep in `knowledge.index` pages the full atom
/// table with `created_at ASC, id ASC`. A recording embedder captures the
/// texts in the order the pages deliver them; with every atom forced onto
/// one shared `created_at` (so the `id` tiebreak is load-bearing), the
/// recorded sequence must equal every atom exactly once in `id ASC` order —
/// no duplicates, no misses, and the exact documented order.
#[tokio::test]
async fn index_reembed_paging_sweep_covers_equal_created_at_in_order() {
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;

    /// Records every text it is asked to embed, in call order, and returns a
    /// fixed vector per text so the vector write succeeds.
    struct RecordingEmbedService {
        recorded: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbeddingService for RecordingEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            self.recorded
                .lock()
                .expect("recorded lock")
                .extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![0.5f32; DIM]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "recording-embed-service"
        }
    }

    struct RecordingEmbedProvider {
        recorded: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbedderProvider for RecordingEmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(RecordingEmbedService {
                recorded: Arc::clone(&self.recorded),
            }))
        }
    }

    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        brain_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    })
    .expect("runtime");
    rt.register_embedder(RecordingEmbedProvider {
        recorded: Arc::clone(&recorded),
    });
    let f = pack(rt.clone());

    let content = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
    for n in 0..31 {
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": format!("reembed-{n:03}"), "name": format!("ReEmbed {n:03}"), "content": content }] }),
        )
        .await
        .expect("upsert atom");
    }

    // Force every atom onto one shared `created_at` so the paging sweep
    // exercises the tiebreak path; otherwise unique microsecond timestamps
    // would let the test pass even without the `id` tiebreak.
    let shared_created_at = 1_750_000_000_000_000_i64;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer must open");
        writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_atoms SET created_at = ?1".into(),
                params: vec![SqlValue::Integer(shared_created_at)],
                label: None,
            })
            .await
            .expect("force shared created_at");
    }
    // One shared `created_at` reduces the documented `created_at ASC,
    // id ASC` page order to `id ASC`; read (name, id) straight from the
    // table and sort by id in Rust rather than re-deriving with SQL. The
    // writer guard above is dropped before this reader opens.
    let expected_names: Vec<String> = {
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("sql reader must open");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT id, name FROM knowledge_atoms WHERE deleted_at IS NULL".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("read atom rows");
        let mut pairs: Vec<(String, String)> = rows
            .iter()
            .map(|row| {
                let id = match row.get("id") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected id {other:?}"),
                };
                let name = match row.get("name") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected name {other:?}"),
                };
                (id, name)
            })
            .collect();
        pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        pairs.into_iter().map(|(_, name)| name).collect()
    };
    assert_eq!(expected_names.len(), 31, "all atoms must be seeded");

    let result = f
        .dispatch("knowledge.index", json!({ "batch_size": 7 }))
        .await
        .expect("index ok");
    assert_eq!(
        result["indexed"].as_u64(),
        Some(31),
        "every atom must be indexed by the default engine: {result:?}"
    );

    // `atom_embed_text` puts the atom name first, so the first line of each
    // recorded text is the atom name; the concatenated pages must deliver
    // every atom exactly once in the documented page order.
    let recorded_names: Vec<String> = recorded
        .lock()
        .expect("recorded lock")
        .iter()
        .map(|text| {
            text.lines()
                .next()
                .expect("embed text carries the atom name")
                .to_string()
        })
        .collect();
    assert_eq!(
        recorded_names, expected_names,
        "re-embed paging sweep must embed every atom exactly once in \
         created_at ASC, id ASC order"
    );
}

// ── delete_atoms ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_atoms_soft_deletes_by_slug() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "to-delete", "name": "Will be gone", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let del_resp = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": ["to-delete"] }))
        .await
        .expect("delete ok");

    assert_eq!(del_resp["deleted"], 1);

    // get should now return not-found.
    let err = f
        .dispatch("knowledge.get", json!({ "id": "to-delete" }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("NotFound"),
        "expected not-found after delete, got: {err}"
    );
}

#[tokio::test]
async fn delete_atoms_returns_zero_for_unknown_slug() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.delete_atoms",
            json!({ "ids": ["does-not-exist"] }),
        )
        .await
        .expect("delete ok even for missing");
    assert_eq!(resp["deleted"], 0);
}

#[tokio::test]
async fn delete_atoms_rejects_domain_mirror_slug_without_mutation() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "retrieval", "name": "Retrieval", "description": "Dense and sparse retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —" }] }),
    )
    .await
    .expect("upsert domain");

    let err = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": ["retrieval"] }))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // Direct lookup and search must agree: the domain is present on both paths.
    let got = f
        .dispatch("knowledge.get", json!({ "id": "retrieval" }))
        .await
        .expect("get must still resolve the domain");
    assert_eq!(got["kind"], "domain");
    assert!(got["members"].is_array());

    let search = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["slug"] == "retrieval"),
        "search must still find the domain: {results:?}"
    );
}

#[tokio::test]
async fn delete_atoms_rejects_domain_mirror_uuid_without_mutation() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "embedding-theory", "name": "Embedding Theory", "description": "vector embedding concepts — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts" }] }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "embedding-theory" }))
        .await
        .expect("get domain by slug");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    let err = f
        .dispatch("knowledge.delete_atoms", json!({ "ids": [uuid.clone()] }))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    // Direct lookup by UUID must agree with search: both say the domain is present.
    let got = f
        .dispatch("knowledge.get", json!({ "id": uuid }))
        .await
        .expect("get must still resolve the domain by uuid");
    assert_eq!(got["kind"], "domain");

    let search = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "embedding", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["slug"] == "embedding-theory"),
        "search must still find the domain: {results:?}"
    );
}

#[tokio::test]
async fn delete_atoms_mixed_request_with_domain_mirror_leaves_normal_atom_live() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "normal-atom", "name": "Normal Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("seed atom");
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "mixed-domain", "name": "Mixed Domain", "description": "Mixed domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("seed domain");

    let err = f
        .dispatch(
            "knowledge.delete_atoms",
            json!({ "ids": ["normal-atom", "mixed-domain"] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "normal-atom" }))
        .await
        .expect("normal atom must remain live after the rejected mixed request");
    assert_eq!(atom["kind"], "atom");
}

// ── stats ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stats_reflects_current_corpus() {
    let f = pack(rt());
    // Empty corpus.
    let empty = f
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("stats ok");
    assert_eq!(empty["total_atoms"], 0);
    assert_eq!(empty["total_domains"], 0);

    // Add atoms.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "a1", "name": "Alpha", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "a2", "name": "Beta", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": false },
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "d1", "name": "Domain1", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("stats ok 2");
    assert_eq!(resp["total_atoms"], 2);
    assert_eq!(resp["total_domains"], 1);
    // 1 of 2 atoms is finalized → eval_coverage = 0.5.
    let cov = resp["eval_coverage"].as_f64().expect("eval_coverage f64");
    assert!(
        (cov - 0.5).abs() < 1e-6,
        "expected eval_coverage=0.5, got {cov}"
    );
}

// ── fold ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fold_selects_within_budget() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [
                    { "id": "c1", "score": 0.9, "size": 100 },
                    { "id": "c2", "score": 0.8, "size": 200 },
                    { "id": "c3", "score": 0.7, "size": 150 },
                    { "id": "c4", "score": 0.6, "size": 50 },
                ],
                "budget": 300
            }),
        )
        .await
        .expect("fold ok");

    let selected = resp["selected"].as_array().expect("selected array");
    let total_size = resp["total_size"].as_u64().expect("total_size");
    assert!(
        total_size <= 300,
        "total_size {total_size} must not exceed budget 300"
    );
    assert!(!selected.is_empty(), "at least one item should be selected");
    assert_eq!(resp["budget"], 300);
}

#[tokio::test]
async fn fold_empty_candidates_returns_empty_selection() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({ "candidates": [], "budget": 1000 }),
        )
        .await
        .expect("fold empty ok");

    let selected = resp["selected"].as_array().expect("selected array");
    assert!(selected.is_empty());
    assert_eq!(resp["total_size"], 0);
}

#[tokio::test]
async fn fold_respects_min_score_filter() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [
                    { "id": "high", "score": 0.9, "size": 100 },
                    { "id": "low",  "score": 0.2, "size": 100 },
                ],
                "budget": 10000,
                "min_score": 0.5
            }),
        )
        .await
        .expect("fold ok");

    let selected = resp["selected"].as_array().expect("selected");
    let ids: Vec<&str> = selected.iter().filter_map(|v| v["id"].as_str()).collect();
    assert!(
        ids.contains(&"high"),
        "high-score item should be selected: {ids:?}"
    );
    assert!(
        !ids.contains(&"low"),
        "low-score item should be filtered: {ids:?}"
    );
}

// ── knowledge.search ──────────────────────────────────────────────────────────

/// Seed 10 atoms with realistic content for search tests.
async fn seed_search_corpus(f: &Fixture) {
    let atoms = json!({
        "atoms": [
            { "slug": "rag",             "name": "RAG",               "content": "Retrieval-Augmented Generation combines retrieval with generation — covering concepts techniques algorithms implementations applications use cases and design patterns in detail RAG retrieves relevant passages before generating text dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "tags": ["retrieval", "rag"], "finalized": true },
            { "slug": "lora",            "name": "LoRA",              "content": "Low-Rank Adaptation of large language models — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["fine-tuning", "adapter"], "finalized": true },
            { "slug": "flash-attention", "name": "FlashAttention",    "content": "Memory-efficient attention using tiling — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering", "tags": ["attention", "gpu"], "finalized": true },
            { "slug": "gqa",             "name": "GQA",               "content": "Grouped Query Attention reduces KV cache — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["attention", "inference"], "finalized": true },
            { "slug": "rope",            "name": "RoPE",              "content": "Rotary Position Embedding for transformers — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —", "tags": ["embedding", "position"], "finalized": true },
            { "slug": "agent",           "name": "Agent",             "content": "Autonomous agent using LLM tool calls — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["agent", "tool-use"], "finalized": true },
            { "slug": "chain-of-thought","name": "Chain-of-Thought",  "content": "Prompting technique for step-by-step reasoning — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —", "tags": ["reasoning", "prompting"], "finalized": true },
            { "slug": "speculative",     "name": "Speculative Decoding", "content": "Draft model accelerates inference via speculation — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["inference", "draft"], "finalized": true },
            { "slug": "quantization",    "name": "Quantization",     "content": "Reduce model size by lowering numerical precision — covering concepts techniques algorithms implementations applications use cases and design patterns in", "tags": ["compression", "inference"], "finalized": true },
            { "slug": "dpo",             "name": "DPO",               "content": "Direct Preference Optimization for RLHF alignment — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["fine-tuning", "alignment"], "finalized": true },
        ]
    });
    f.dispatch("knowledge.upsert_atoms", atoms)
        .await
        .expect("seed atoms");
}

#[tokio::test]
async fn search_basic_returns_ranked_results() {
    let f = pack(rt());
    seed_search_corpus(&f).await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval generation", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "expected some results");

    // RAG should rank highly for "retrieval generation".
    let first_name = results[0]["name"].as_str().unwrap_or("");
    assert_eq!(
        first_name, "RAG",
        "RAG should rank first for 'retrieval generation', got: {results:?}"
    );
}

#[tokio::test]
async fn search_exact_name_bonus_surfaces_exact_match_first() {
    let f = pack(rt());
    seed_search_corpus(&f).await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "LoRA", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "expected results for LoRA");
    let first_name = results[0]["name"].as_str().unwrap_or("");
    assert_eq!(
        first_name, "LoRA",
        "exact name match LoRA should rank first"
    );
}

#[tokio::test]
async fn search_query_expansion_matches_related_form() {
    let f = pack(rt());
    // "agents" expands to "agent" via plural stripping.
    seed_search_corpus(&f).await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "agents", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results array");
    // Agent atom should appear in results.
    let names: Vec<&str> = results.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(
        names.contains(&"Agent"),
        "expected Agent in search results for 'agents', got: {names:?}"
    );
}

#[tokio::test]
async fn search_weight_override_changes_ranking() {
    let f = pack(rt());
    seed_search_corpus(&f).await;

    // With very high w_tags weight, the result tagged "attention" should rank first for "attention".
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "attention",
                "weights": { "w_tags": 50.0, "w_name": 1.0, "w_content": 0.1 },
                "rerank": false
            }),
        )
        .await
        .expect("search ok with weights");

    let results = resp["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "expected results");
    // FlashAttention or GQA have tag "attention".
    let first_name = results[0]["name"].as_str().unwrap_or("");
    assert!(
        first_name == "FlashAttention" || first_name == "GQA",
        "expected attention-tagged atom first, got: {first_name}"
    );
}

#[tokio::test]
async fn search_limit_is_respected() {
    let f = pack(rt());
    seed_search_corpus(&f).await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "inference", "limit": 2, "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results array");
    assert!(
        results.len() <= 2,
        "expected at most 2 results, got {}",
        results.len()
    );
}

#[tokio::test]
async fn search_empty_corpus_returns_empty_results() {
    let f = pack(rt());
    // No atoms seeded.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "anything", "rerank": false }),
        )
        .await
        .expect("search ok on empty corpus");

    let results = resp["results"].as_array().expect("results array");
    assert!(results.is_empty(), "empty corpus should return no results");
}

#[tokio::test]
async fn search_rejects_empty_query() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.search", json!({ "query": "  " }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("query must not be empty"),
        "got: {err}"
    );
}

#[tokio::test]
async fn search_rejects_invalid_kind_value() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "attention", "kind": "atoms", "rerank": false }),
        )
        .await
        .expect_err("unknown kind must fail closed");
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "must be InvalidInput, got: {err:?}"
    );
    assert!(
        err.to_string()
            .contains("kind must be one of: atom, domain"),
        "got: {err}"
    );
}

#[tokio::test]
async fn search_rejects_invalid_exclude_status_value() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "attention",
                "exclude_status": "deprectaed",
                "rerank": false
            }),
        )
        .await
        .expect_err("unknown exclusion status must fail closed");
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "must be InvalidInput, got: {err:?}"
    );
    assert!(
        err.to_string()
            .contains("exclude_status must be one of: draft, reviewed, deprecated"),
        "got: {err}"
    );
}

#[tokio::test]
async fn search_type_filter_returns_only_atoms() {
    let f = pack(rt());
    seed_search_corpus(&f).await;
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "attention-domain", "name": "Attention Domain", "description": "covers attention methods — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts" }] }),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "attention", "type": "atom", "rerank": false }),
        )
        .await
        .expect("search filtered ok");

    let results = resp["results"].as_array().expect("results array");
    for r in results {
        assert_eq!(
            r["kind"].as_str().unwrap_or(""),
            "atom",
            "all results should be atoms when type=atom: {r}"
        );
    }
}

#[tokio::test]
async fn search_type_domain_finds_upserted_domains() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [
            { "slug": "retrieval-methods", "name": "Retrieval Methods", "description": "Dense and sparse retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —" }
        ]}),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search domain ok");

    let results = resp["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "domain search should find the upserted domain"
    );
    assert_eq!(results[0]["kind"].as_str().unwrap_or(""), "domain");
}

// ── suggest ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn suggest_returns_domains_for_query() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "retrieval-methods", "name": "Retrieval Methods", "description": "sparse and dense retrieval techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —" },
                { "slug": "embedding-theory", "name": "Embedding Theory", "description": "vector embedding concepts — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts" },
            ]
        }),
    )
    .await
    .expect("upsert domains");

    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "retrieval techniques for dense and sparse methods" }),
        )
        .await
        .expect("suggest ok");

    let results = resp["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "suggest should return at least one domain"
    );
    let first = &results[0];
    assert!(first["id"].is_string(), "result must have id");
    assert!(first["name"].is_string(), "result must have name");
    assert!(first["score"].is_number(), "result must have score");
}

#[tokio::test]
async fn suggest_rejects_empty_query() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.suggest", json!({ "query": "" }))
        .await
        .expect_err("empty query should fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

// ── compose ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn compose_returns_markdown_for_atoms() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rag-overview",
                    "name": "RAG Overview",
                    "content": "Retrieval-augmented generation combines retrieval with generation. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "dense-retrieval",
                    "name": "Dense Retrieval",
                    "content": "Dense retrieval uses vector embeddings to find relevant documents. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["rag-overview", "dense-retrieval"],
                "query": "retrieval augmented generation"
            }),
        )
        .await
        .expect("compose ok");

    let md = resp["data"]["markdown"].as_str().expect("markdown");
    assert!(
        md.contains("Knowledge Briefing"),
        "markdown must have heading"
    );
    let atoms = resp["data"]["atoms"].as_array().expect("atoms array");
    assert_eq!(atoms.len(), 2, "expected 2 atoms in response");
    let count = resp["data"]["count"].as_u64().expect("count");
    assert_eq!(count, 2);
}

/// #1505: the public namespace parameter is an exact compose scope, not a
/// widened visible set. Identical slugs in local and a measurement arm must
/// resolve to the arm's atom only, while an absent parameter preserves the
/// unchanged local default.
#[tokio::test]
async fn compose_namespace_selects_exact_atom_corpus() {
    let f = pack(rt());
    let shared_slug = "namespace-compose-target";

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": shared_slug,
                "name": "Local Compose Marker",
                "content": "Local corpus marker for namespace composition regression coverage with enough distinct retrieval words to satisfy the atom content validation contract and remain readable in generated markdown output."
            }]
        }),
    )
    .await
    .expect("upsert local atom");
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "namespace": "bench-arm-a",
            "atoms": [{
                "slug": shared_slug,
                "name": "Bench Arm Compose Marker",
                "content": "Bench arm corpus marker for exact namespace composition regression coverage with enough distinct retrieval words to satisfy the atom content validation contract and remain readable in generated markdown output."
            }]
        }),
    )
    .await
    .expect("upsert bench-arm atom");

    let arm = f
        .dispatch(
            "knowledge.compose",
            json!({
                "namespace": "bench-arm-a",
                "atom_ids": [shared_slug],
                "query": "namespace composition marker",
            }),
        )
        .await
        .expect("compose exact bench-arm namespace");
    let arm_atom = &arm["data"]["atoms"][0];
    assert_eq!(arm_atom["name"], json!("Bench Arm Compose Marker"));
    assert!(arm["data"]["markdown"]
        .as_str()
        .expect("arm markdown")
        .contains("Bench arm corpus marker"));
    assert!(
        !arm["data"]["markdown"]
            .as_str()
            .expect("arm markdown")
            .contains("Local corpus marker"),
        "exact arm compose must not blend the same slug from local"
    );

    let local = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": [shared_slug],
                "query": "namespace composition marker",
            }),
        )
        .await
        .expect("compose unchanged local default");
    assert_eq!(
        local["data"]["atoms"][0]["name"],
        json!("Local Compose Marker")
    );
}

/// ADR-096/#1505: nested brain.resolve/profile calls must carry the outer
/// request's principal and exact namespace, not the registry's baked daemon
/// identity. The gate denies either nested read under any other actor.
#[tokio::test]
async fn compose_nested_profile_reads_preserve_request_identity() {
    use khive_pack_brain::BrainPack;

    let rt = rt();
    let gate = Arc::new(NestedProfileIdentityGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    builder.with_gate(gate.clone());
    builder.with_actor_id(Some("daemon".to_string()));
    let registry = builder.build().expect("registry");
    let arm_ns = "bench-arm-a";

    registry
        .dispatch(
            "brain.create_profile",
            json!({
                "namespace": arm_ns,
                "name": "requester-compose-v1",
                "consumer_kind": "knowledge_compose",
            }),
        )
        .await
        .expect("create arm profile");
    registry
        .dispatch(
            "brain.activate",
            json!({
                "namespace": arm_ns,
                "profile_id": "requester-compose-v1",
            }),
        )
        .await
        .expect("activate arm profile");
    registry
        .dispatch(
            "brain.bind",
            json!({
                "namespace": arm_ns,
                "profile_id": "requester-compose-v1",
                "consumer_kind": "knowledge_compose",
            }),
        )
        .await
        .expect("bind arm profile");
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "namespace": arm_ns,
                "atoms": [{
                    "slug": "requester-compose-atom",
                    "name": "Requester Compose Atom",
                    "content": "Per-request identity propagation for nested profile reads with enough retrieval corpus words to satisfy validation and produce a deterministic briefing.",
                }]
            }),
        )
        .await
        .expect("upsert arm atom");
    gate.requests.lock().expect("gate request lock").clear();

    registry
        .dispatch_with_identity(
            "knowledge.compose",
            json!({
                "namespace": arm_ns,
                "atom_ids": ["requester-compose-atom"],
                "query": "per request identity propagation",
            }),
            Some(RequestIdentity {
                namespace: "local".to_string(),
                actor_id: Some("requester".to_string()),
                visible_namespaces: vec!["local".to_string()],
                process_ref: None,
                request_id: Some(1505),
            }),
        )
        .await
        .expect("compose with request-scoped nested profile reads");

    let requests = gate.requests.lock().expect("gate request lock");
    let nested: Vec<_> = requests
        .iter()
        .filter(|(verb, _, _)| matches!(verb.as_str(), "brain.resolve" | "brain.profile"))
        .collect();
    assert_eq!(
        nested.len(),
        2,
        "bound compose must perform both nested profile reads: {requests:?}"
    );
    assert!(
        nested
            .iter()
            .all(|(_, actor, namespace)| actor == "requester" && namespace == arm_ns),
        "nested Gate checks must preserve requester + exact arm identity: {nested:?}"
    );
}

#[tokio::test]
async fn compose_returns_markdown_for_domain() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "atom-a", "name": "Atom A", "content": "content of atom a dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                {
                    "slug": "test-domain",
                    "name": "Test Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                    "members": ["atom-a"]
                }
            ]
        }),
    )
    .await
    .expect("upsert domain");

    let domain_resp = f
        .dispatch("knowledge.get", json!({ "id": "test-domain" }))
        .await
        .expect("get domain");
    let domain_id = domain_resp["id"].as_str().expect("domain id");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "domain_ids": [domain_id],
                "query": "content"
            }),
        )
        .await
        .expect("compose from domain ok");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert!(
        !atoms.is_empty(),
        "compose from domain should include member atoms"
    );
}

/// PR #816: `knowledge.compose` accepts compact hex prefixes
/// for domains but must normalize them (`hex_prefix_to_uuid_pattern`) before
/// binding the `LIKE` pattern — a >8-char compact prefix could not match the
/// hyphenated `id` column before the fix.
#[tokio::test]
async fn compose_resolves_domain_by_compact_hex_prefix_over_8_chars() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "atom-a", "name": "Atom A", "content": "content of atom a dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                {
                    "slug": "test-domain",
                    "name": "Test Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                    "members": ["atom-a"]
                }
            ]
        }),
    )
    .await
    .expect("upsert domain");

    let domain_resp = f
        .dispatch("knowledge.get", json!({ "id": "test-domain" }))
        .await
        .expect("get domain");
    let domain_id = domain_resp["id"].as_str().expect("domain id").to_string();
    let compact = domain_id.replace('-', "");
    let prefix = &compact[..16];

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "domain_ids": [prefix],
                "query": "content"
            }),
        )
        .await
        .expect("compose from compact domain prefix must resolve");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert!(
        !atoms.is_empty(),
        "compose from compact domain prefix should include member atoms"
    );
}

/// PR #816: same normalization requirement for atom ids.
#[tokio::test]
async fn compose_resolves_atom_by_compact_hex_prefix_over_8_chars() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rag-overview",
                    "name": "RAG Overview",
                    "content": "Retrieval-augmented generation combines retrieval with generation. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    let atom_resp = f
        .dispatch("knowledge.get", json!({ "id": "rag-overview" }))
        .await
        .expect("get atom");
    let atom_id = atom_resp["id"].as_str().expect("atom id").to_string();
    let compact = atom_id.replace('-', "");
    let prefix = &compact[..16];

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": [prefix],
                "query": "retrieval augmented generation"
            }),
        )
        .await
        .expect("compose from compact atom prefix must resolve");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert_eq!(atoms.len(), 1, "expected exactly 1 atom");
    assert_eq!(
        atoms[0]["slug"].as_str(),
        Some("rag-overview"),
        "compact atom prefix must resolve to the correct atom"
    );
}

/// PR #816 (precedence): an all-hex slug must still win over
/// prefix interpretation. Atom B's slug is deliberately set to the same
/// hex string as the first 16 chars of atom A's compact id — a value that
/// is *also* a syntactically valid compact prefix. Because `compose` tries
/// an exact slug match before falling back to prefix scanning, this must
/// resolve atom B (by slug), never atom A (by prefix collision).
#[tokio::test]
async fn compose_atom_all_hex_slug_wins_over_prefix_interpretation() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "atom-x", "name": "Atom X", "content": "atom x content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom x");

    let atom_x_resp = f
        .dispatch("knowledge.get", json!({ "id": "atom-x" }))
        .await
        .expect("get atom x");
    let atom_x_id = atom_x_resp["id"].as_str().expect("atom x id").to_string();
    let hex_slug = atom_x_id.replace('-', "")[..16].to_string();

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": hex_slug.clone(), "name": "Atom Y", "content": "atom y content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atom y with all-hex slug");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": [hex_slug.clone()],
                "query": "atom content"
            }),
        )
        .await
        .expect("compose by all-hex slug must resolve");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms");
    assert_eq!(atoms.len(), 1, "expected exactly 1 atom");
    assert_eq!(
        atoms[0]["slug"].as_str(),
        Some(hex_slug.as_str()),
        "all-hex slug must resolve to the atom with that slug, not a prefix collision"
    );
    assert_eq!(
        atoms[0]["name"].as_str(),
        Some("Atom Y"),
        "must resolve atom Y by slug, never atom X by prefix interpretation"
    );
}

#[tokio::test]
async fn compose_rejects_missing_ids() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.compose", json!({ "query": "test" }))
        .await
        .expect_err("compose with no ids should fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

#[tokio::test]
async fn compose_rejects_empty_query() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.compose",
            json!({ "atom_ids": ["some-atom"], "query": "" }),
        )
        .await
        .expect_err("empty query should fail");
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

#[tokio::test]
async fn suggest_returns_empty_when_no_domains_present() {
    let f = pack(rt());
    // Empty corpus: no domains upserted. suggest should succeed with an empty results array.
    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "anything related to general knowledge retrieval methods" }),
        )
        .await
        .expect("suggest on empty corpus must not crash");
    let results = resp["results"].as_array().expect("results array");
    assert!(
        results.is_empty(),
        "no domains in corpus → empty results, got: {results:?}"
    );
}

#[tokio::test]
async fn suggest_honors_limit_param() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "domain-one", "name": "Domain One", "description": "first domain about retrieval — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering" },
                { "slug": "domain-two", "name": "Domain Two", "description": "second domain about search — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering" },
                { "slug": "domain-three", "name": "Domain Three", "description": "third domain about indexing — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering" },
            ]
        }),
    )
    .await
    .expect("upsert domains");

    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "domain retrieval search indexing methods and techniques", "limit": 1 }),
        )
        .await
        .expect("suggest with limit=1");

    let results = resp["results"].as_array().expect("results array");
    // All 3 seeded domains match the FTS phrase "domain"; suggest truncates to
    // exactly `limit` via hits.truncate(limit) before returning.
    assert_eq!(
        results.len(),
        1,
        "limit=1 with 3 matching domains must return exactly 1 result, got: {}",
        results.len()
    );
}

#[tokio::test]
async fn compose_accepts_mix_of_domain_ids_and_atom_ids() {
    let f = pack(rt());

    // Atom directly referenced by atom_ids.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "direct-atom", "name": "Direct Atom", "content": "directly specified atom content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "member-atom", "name": "Member Atom", "content": "member atom from domain dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    // Domain whose member provides member-atom.
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [
                { "slug": "mix-domain", "name": "Mix Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "members": ["member-atom"] }
            ]
        }),
    )
    .await
    .expect("upsert domain");

    let domain_resp = f
        .dispatch("knowledge.get", json!({ "id": "mix-domain" }))
        .await
        .expect("get domain");
    let domain_id = domain_resp["id"].as_str().expect("domain id");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "domain_ids": [domain_id],
                "atom_ids": ["direct-atom"],
                "query": "content"
            }),
        )
        .await
        .expect("compose with mix of domain_ids and atom_ids");

    let atoms = resp["data"]["atoms"].as_array().expect("atoms array");
    assert_eq!(
        atoms.len(),
        2,
        "compose with 1 domain member + 1 direct atom should yield 2 atoms (deduped), got: {atoms:?}"
    );
    let count = resp["data"]["count"].as_u64().expect("count");
    assert_eq!(count, 2);
}

// ── compose slim output (explain flag) ───────────────────────────────────────

#[tokio::test]
async fn compose_default_omits_sections_and_score_annotations() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "slim-atom-a",
                    "name": "Slim Atom A",
                    "content": "retrieval augmented generation dense sparse corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["slim-atom-a"],
                "query": "retrieval augmented generation"
            }),
        )
        .await
        .expect("compose default ok");

    let data = &resp["data"];
    assert!(
        data.get("sections").is_none(),
        "sections must be absent in default mode"
    );
    assert!(
        data.get("section_count").is_none(),
        "section_count must be absent in default mode"
    );

    let md = data["markdown"].as_str().expect("markdown");
    assert!(
        !md.contains("(score:"),
        "markdown must not contain (score: in default mode"
    );
    assert!(
        !md.contains("Score:"),
        "markdown must not contain Score: in default mode"
    );

    let atoms = data["atoms"].as_array().expect("atoms array");
    assert!(!atoms.is_empty(), "atoms must be present");
    let score_val = atoms[0]["score"].as_f64().expect("score is a number");
    let rendered = format!("{}", atoms[0]["score"]);
    let decimal_len = rendered
        .find('.')
        .map(|dot| rendered.len() - dot - 1)
        .unwrap_or(0);
    assert!(
        decimal_len <= 4,
        "atom score must serialize with at most 4 decimal places, got: {rendered}"
    );
    let _ = score_val;
}

#[tokio::test]
async fn compose_explain_true_atom_path_includes_score_in_markdown() {
    // This test uses a no-embedder runtime (rt()). Without an embedder,
    // embed_query() returns None, so section_results is always empty and
    // compose falls through to the atom-path markdown branch. The sole
    // assertion here is that explain=true causes "Score:" to appear in the
    // atom-path output. The section path (sections[] + breakdown + section_count
    // + "(score:") is exercised by the embedder-backed test in fixes.rs:
    // compose_explain_sections::compose_explain_true_section_path_is_exercised.
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "explain-atom-b",
                    "name": "Explain Atom B",
                    "content": "retrieval augmented generation combines dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }
            ]
        }),
    )
    .await
    .expect("upsert atom");

    let resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "atom_ids": ["explain-atom-b"],
                "query": "retrieval augmented generation dense sparse",
                "explain": true
            }),
        )
        .await
        .expect("compose explain ok");

    let data = &resp["data"];
    let md = data["markdown"].as_str().expect("markdown");

    // Without an embedder, sections are never emitted — the atom-path branch
    // runs and renders "Score: X.XXXX" per atom when explain=true.
    assert!(
        data.get("sections").is_none(),
        "no-embedder runtime must not emit sections key"
    );
    assert!(
        md.contains("Score:"),
        "atom-path markdown must contain 'Score:' when explain=true, got: {md}"
    );
}

// ── KPK-002: DomainInput deny_unknown_fields + domain-mirror content-word minimum ──

#[tokio::test]
async fn kpk002_domain_input_rejects_unknown_fields() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "test-domain",
                    "name": "Test Domain",
                    "description": "A domain with enough words to pass the twenty word minimum content requirement for testing.",
                    "unknown_field_xyz": "should cause rejection"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown_field_xyz") || msg.contains("unknown field"),
        "unknown field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn kpk002_domain_mirror_atom_below_word_minimum_is_rejected() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "sparse-domain",
                    "name": "Sparse Domain",
                    "description": "Too short"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("20") || msg.contains("words") || msg.contains("content"),
        "description below 20-word minimum must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn kpk002_domain_mirror_atom_empty_description_is_rejected() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "empty-desc-domain",
                    "name": "Empty Desc Domain"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("20") || msg.contains("words") || msg.contains("content"),
        "missing description must be rejected as below 20-word minimum; got: {msg}"
    );
}

#[tokio::test]
async fn kpk002_domain_with_sufficient_description_is_accepted() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "rich-domain",
                    "name": "Rich Domain",
                    "description": "This domain covers retrieval augmented generation patterns for building scalable knowledge systems with structured graph storage and semantic search capabilities for AI agents.",
                    "tags": ["rag", "retrieval"],
                    "members": []
                }]
            }),
        )
        .await
        .expect("domain with sufficient description must be accepted");
    assert_eq!(resp["created"], json!(1u64));
    assert_eq!(resp["updated"], json!(0u64));
}

// ── Secret-gate regression tests ─────────────────────────────────────────────

fn is_secret_detected(err: &RuntimeError) -> bool {
    matches!(err, RuntimeError::SecretDetected(_))
}

/// knowledge.upsert_domains with a credential-shaped slug must be rejected.
#[tokio::test]
async fn upsert_domains_blocks_secret_in_slug_insert() {
    let f = pack(rt());
    let result = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "ghp_FakeGitHubToken0000000000000000000", // gitleaks:allow
                    "name": "Secret Slug Domain",
                    "description": "This domain describes retrieval augmented generation patterns for building scalable AI knowledge systems with structured graph storage and semantic search capabilities.",
                }]
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "upsert_domains with secret in slug must be rejected; got: {result:?}"
    );
}

/// knowledge.upsert_domains with a clean slug must succeed.
#[tokio::test]
async fn upsert_domains_clean_slug_passes() {
    let f = pack(rt());
    let result = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "clean-domain-slug",
                    "name": "Clean Domain",
                    "description": "This domain covers retrieval augmented generation patterns for AI knowledge systems at scale with hybrid search and graph traversal features.",
                }]
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "upsert_domains with clean slug must succeed; got: {result:?}"
    );
}

// ── #441: soft-deleted slugs cannot be upserted again ────────────────────────

/// Soft-deleting an atom's slug and then upserting the same slug again must
/// return a clean lifecycle error, never a raw SQLite unique-constraint error.
#[tokio::test]
async fn upsert_atoms_rejects_reuse_of_soft_deleted_slug_cleanly() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "draft-guide", "name": "Draft Guide", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("seed atom");

    f.dispatch("knowledge.delete_atoms", json!({ "ids": ["draft-guide"] }))
        .await
        .expect("soft delete atom");

    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{ "slug": "draft-guide", "name": "Draft Guide Reborn", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("deleted") || msg.contains("previously deleted"),
        "error should explain the slug was previously deleted: {msg}"
    );
    let lower = msg.to_lowercase();
    assert!(
        !lower.contains("unique") && !lower.contains("constraint") && !lower.contains("sqlite"),
        "no raw SQLite unique-constraint wording may leak to the caller: {msg}"
    );
}

/// Soft-deleting a domain (via the generic delete verb, which tombstones both
/// the canonical domain row and its mirror atom) and then upserting the same
/// slug again must return a clean lifecycle error, never a raw SQLite error.
#[tokio::test]
async fn upsert_domains_rejects_reuse_of_soft_deleted_slug_cleanly() {
    let f = pack_via_registry(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "draft-domain", "name": "Draft Domain", "description": "Draft domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
    )
    .await
    .expect("seed domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "draft-domain" }))
        .await
        .expect("get domain before delete");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Soft-delete via the generic delete verb, not knowledge.delete_atoms.
    f.dispatch("delete", json!({ "id": uuid }))
        .await
        .expect("generic soft delete domain");

    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({ "domains": [{ "slug": "draft-domain", "name": "Draft Domain Reborn", "description": "Draft domain techniques — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering concepts techniques" }] }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("deleted") || msg.contains("previously deleted"),
        "error should explain the slug was previously deleted: {msg}"
    );
    let lower = msg.to_lowercase();
    assert!(
        !lower.contains("unique")
            && !lower.contains("constraint")
            && !lower.contains("sqlite")
            && !lower.contains("knowledge_domains")
            && !lower.contains("knowledge_atoms"),
        "no raw SQLite unique-constraint wording may leak to the caller: {msg}"
    );

    // The rejected reuse must not resurrect the tombstoned domain.
    let not_found = f
        .dispatch("knowledge.get", json!({ "id": "draft-domain" }))
        .await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after rejected reuse, got: {not_found:?}"
    );
}

// ── resolver e2e tests (ADR-061): registry wired via PackRegistry::register_packs ────────

/// Build a VerbRegistry the same way the production MCP server does: via
/// `PackRegistry::register_packs`. This path calls `create_resolver` and wires
/// the knowledge `PackByIdResolver` into the registry, so generic `get` /
/// `delete` / `update` can reach knowledge-private tables.
fn pack_via_registry(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".to_string(), "knowledge".to_string()],
        rt.clone(),
        &mut builder,
    )
    .expect("register_packs must succeed for kg+knowledge");
    let registry = builder.build().expect("registry build");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

/// Generic `get(id=<atom-uuid>)` via the resolver returns the same wire shape
/// as `knowledge.get(id=<slug>)`: tags is a JSON array, properties is a JSON
/// object (or null), and created_at/updated_at are ISO 8601 strings.
#[tokio::test]
async fn resolver_generic_get_atom_returns_public_wire_shape() {
    let f = pack_via_registry(rt());

    // Create an atom with tags and properties.
    let upsert = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "resolver-atom-e2e",
                    "name": "Resolver Atom E2E",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                    "tags": ["test", "resolver"],
                    "properties": { "source": "test" }
                }]
            }),
        )
        .await
        .expect("upsert atom");
    assert_eq!(upsert["created"], 1);

    // Fetch the UUID from knowledge.get by slug.
    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-atom-e2e" }))
        .await
        .expect("knowledge.get by slug");
    let uuid = by_slug["id"].as_str().expect("id string");

    // Now fetch via generic get using the UUID — this exercises the resolver path.
    let by_uuid = f
        .dispatch("get", json!({ "id": uuid }))
        .await
        .expect("generic get by uuid");

    // kind must be atom.
    assert_eq!(by_uuid["kind"], "atom", "wrong kind: {by_uuid}");
    assert_eq!(by_uuid["slug"], "resolver-atom-e2e");
    assert_eq!(by_uuid["name"], "Resolver Atom E2E");

    // tags must be a JSON array, not a comma-separated string.
    let tags = by_uuid["tags"]
        .as_array()
        .expect("tags must be a JSON array");
    assert!(
        tags.iter().any(|t| t == "test"),
        "expected 'test' tag, got: {tags:?}"
    );

    // created_at and updated_at must be ISO 8601 strings, not raw microsecond integers.
    let created_at = by_uuid["created_at"]
        .as_str()
        .expect("created_at must be a string");
    assert!(
        created_at.contains('T'),
        "created_at must be ISO 8601, got: {created_at:?}"
    );
    let updated_at = by_uuid["updated_at"]
        .as_str()
        .expect("updated_at must be a string");
    assert!(
        updated_at.contains('T'),
        "updated_at must be ISO 8601, got: {updated_at:?}"
    );

    // properties must be a JSON object (or null), not a string.
    assert!(
        by_uuid["properties"].is_object() || by_uuid["properties"].is_null(),
        "properties must be object or null, got: {:?}",
        by_uuid["properties"]
    );
}

/// Generic `get(id=<domain-uuid>)` via the resolver returns the public wire
/// shape: tags and members are JSON arrays, timestamps are ISO 8601 strings.
#[tokio::test]
async fn resolver_generic_get_domain_returns_public_wire_shape() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "resolver-domain-e2e",
                "name": "Resolver Domain E2E",
                "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity tags members",
                "members": ["rag", "dense-retrieval"],
                "tags": ["test", "resolver"]
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-domain-e2e" }))
        .await
        .expect("knowledge.get domain by slug");
    let uuid = by_slug["id"].as_str().expect("id string");

    let by_uuid = f
        .dispatch("get", json!({ "id": uuid }))
        .await
        .expect("generic get domain by uuid");

    assert_eq!(by_uuid["kind"], "domain", "wrong kind: {by_uuid}");
    assert_eq!(by_uuid["slug"], "resolver-domain-e2e");

    // tags must be a JSON array, not a raw string.
    let tags = by_uuid["tags"]
        .as_array()
        .expect("tags must be a JSON array");
    assert!(
        tags.iter().any(|t| t == "test"),
        "expected 'test' tag, got: {tags:?}"
    );

    // members must be a JSON array, not a raw string.
    let members = by_uuid["members"]
        .as_array()
        .expect("members must be a JSON array");
    assert!(
        members.iter().any(|m| m == "rag"),
        "expected 'rag' member, got: {members:?}"
    );

    // timestamps must be ISO 8601 strings.
    let created_at = by_uuid["created_at"]
        .as_str()
        .expect("created_at must be a string");
    assert!(
        created_at.contains('T'),
        "created_at must be ISO 8601, got: {created_at:?}"
    );
}

/// Generic `delete(id=<domain-uuid>)` soft-deletes; subsequent generic
/// `get` returns NotFound.
#[tokio::test]
async fn resolver_generic_soft_delete_domain() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "resolver-delete-domain",
                "name": "Resolver Delete Domain",
                "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-delete-domain" }))
        .await
        .expect("get domain before delete");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Soft-delete via generic delete.
    let del = f
        .dispatch("delete", json!({ "id": uuid }))
        .await
        .expect("generic soft delete");
    assert_eq!(del["deleted"], true, "soft delete response: {del}");

    // Generic get must now return NotFound.
    let not_found = f.dispatch("get", json!({ "id": uuid })).await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after soft delete, got: {not_found:?}"
    );
}

/// Generic `delete(id=<atom-uuid>, hard=true)` hard-deletes a live atom.
#[tokio::test]
async fn resolver_generic_hard_delete_atom() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "resolver-hard-delete-atom",
                "name": "Resolver Hard Delete Atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "resolver-hard-delete-atom" }),
        )
        .await
        .expect("get atom before delete");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Hard-delete the live atom directly.
    let hard_del = f
        .dispatch("delete", json!({ "id": uuid, "hard": true }))
        .await
        .expect("generic hard delete");
    assert_eq!(
        hard_del["deleted"], true,
        "hard delete response: {hard_del}"
    );

    // Generic get must now return NotFound.
    let not_found = f.dispatch("get", json!({ "id": uuid })).await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after hard delete, got: {not_found:?}"
    );
}

/// Generic `update(id=<atom-uuid>)` returns InvalidInput because the knowledge
/// pack defers generic update (pack-private records require pack-specific verbs).
#[tokio::test]
async fn resolver_generic_update_atom_returns_invalid_input() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "resolver-update-atom",
                "name": "Resolver Update Atom",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch("knowledge.get", json!({ "id": "resolver-update-atom" }))
        .await
        .expect("get atom");
    let uuid = by_slug["id"].as_str().expect("id string");

    let err = f
        .dispatch("update", json!({ "id": uuid, "name": "New Name" }))
        .await
        .expect_err("update on knowledge atom must return an error");
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "expected InvalidInput, got: {err:?}"
    );
}

/// Hard-delete an atom that has sections via `knowledge.edit`.
///
/// Without the fix this fails with `FOREIGN KEY constraint failed` because
/// `knowledge_sections` has a FK to `knowledge_atoms(id)` without `ON DELETE
/// CASCADE`.
#[tokio::test]
async fn resolver_generic_hard_delete_atom_with_sections() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "hard-delete-atom-with-sections",
                "name": "Hard Delete Atom With Sections",
                "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert atom");

    let by_slug = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "hard-delete-atom-with-sections" }),
        )
        .await
        .expect("get atom before edit");
    let uuid = by_slug["id"].as_str().expect("id string").to_string();

    // Add sections to the atom.
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "hard-delete-atom-with-sections",
            "sections": [{
                "section_type": "overview",
                "content": "This section tests that hard-delete correctly removes dependent knowledge_sections rows before deleting the parent atom to satisfy the foreign key constraint."
            }]
        }),
    )
    .await
    .expect("add section to atom");

    // Hard-delete must succeed even though sections exist.
    let hard_del = f
        .dispatch("delete", json!({ "id": uuid, "hard": true }))
        .await
        .expect("hard delete atom with sections");
    assert_eq!(
        hard_del["deleted"], true,
        "hard delete response: {hard_del}"
    );

    // Generic get must now return NotFound.
    let not_found = f.dispatch("get", json!({ "id": uuid })).await;
    assert!(
        matches!(not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound after hard delete, got: {not_found:?}"
    );
}

/// Hard-delete a domain whose mirror atom has sections via `knowledge.edit`.
///
/// Without the fix the domain row is deleted first and then the mirror atom
/// delete fails with `FOREIGN KEY constraint failed`, leaving a partial delete.
#[tokio::test]
async fn resolver_generic_hard_delete_domain_with_mirror_sections() {
    let f = pack_via_registry(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "hard-delete-domain-with-sections",
                "name": "Hard Delete Domain With Sections",
                "description": "Domain whose mirror atom will have sections before hard-delete to verify that cascade-delete of dependent knowledge_sections rows works correctly here."
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let domain = f
        .dispatch(
            "knowledge.get",
            json!({ "id": "hard-delete-domain-with-sections" }),
        )
        .await
        .expect("get domain before edit");
    let domain_uuid = domain["id"].as_str().expect("id string").to_string();

    // Add sections to the mirror atom (same UUID as the domain).
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "hard-delete-domain-with-sections",
            "sections": [{
                "section_type": "overview",
                "content": "This section tests that hard-delete of a domain removes dependent knowledge_sections rows from the mirror atom before deleting the atom and domain rows."
            }]
        }),
    )
    .await
    .expect("add section to domain mirror atom");

    // Hard-delete the domain — must succeed even though mirror atom has sections.
    let hard_del = f
        .dispatch("delete", json!({ "id": domain_uuid, "hard": true }))
        .await
        .expect("hard delete domain with mirror sections");
    assert_eq!(
        hard_del["deleted"], true,
        "hard delete response: {hard_del}"
    );

    // Domain must now be NotFound.
    let domain_not_found = f.dispatch("get", json!({ "id": domain_uuid })).await;
    assert!(
        matches!(domain_not_found, Err(RuntimeError::NotFound(_))),
        "expected NotFound for domain after hard delete, got: {domain_not_found:?}"
    );
}

// ── ADR-051 Amendment 1: KG entity blending in knowledge.compose ─────────────
//
// A fake `EmbedderProvider` whose vector is a function of a marker substring
// ("zzzquantumfoo") rather than real semantics: texts containing the marker
// embed to [1.0, 0.0], everything else to [0.0, 1.0]. This makes the
// KG-concept-outranks-lore-atom scenario (the ADR-051 Amendment 1 dogfood
// motivation) deterministic without a real embedding model — the query and
// the seeded concept share the marker, atoms/domains never do.
mod kg_blend {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::collections::HashSet;
    use std::sync::Arc;

    const MARKER: &str = "zzzquantumfoo";
    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;

    struct MarkerEmbedService;

    #[async_trait]
    impl EmbeddingService for MarkerEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; DIM];
                    if t.contains(MARKER) {
                        v[0] = 1.0;
                    } else {
                        v[1] = 1.0;
                    }
                    v
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "marker-embed-service"
        }
    }

    struct MarkerEmbedProvider;

    #[async_trait]
    impl EmbedderProvider for MarkerEmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(MarkerEmbedService))
        }
    }

    fn rt_with_marker_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            brain_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        rt.register_embedder(MarkerEmbedProvider);
        rt
    }

    const QUERY: &str = "zzzquantumfoo kv cache paging decode attention retrieval \
        augmented generation dense sparse benchmark corpus";

    const OVERLAP_CONTENT: &str = "kv cache paging decode attention retrieval augmented \
        generation dense sparse benchmark corpus latency gradient descent transformer vector \
        index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

    async fn seed_domain_and_atom(f: &Fixture) -> String {
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": OVERLAP_CONTENT, "finalized": true }
                ]
            }),
        )
        .await
        .expect("upsert atom");

        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "kg-blend-domain",
                        "name": "KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["kg-blend-atom"]
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain");

        let domain_resp = f
            .dispatch("knowledge.get", json!({ "id": "kg-blend-domain" }))
            .await
            .expect("get domain");
        domain_resp["id"].as_str().expect("domain id").to_string()
    }

    async fn seed_kg_concept(f: &Fixture) -> String {
        let resp = f
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "ZipCache",
                    "description": format!(
                        "{MARKER} paged KV cache quantization technique for decode-time attention"
                    ),
                }),
            )
            .await
            .expect("create kg concept");
        resp["id"].as_str().expect("entity id").to_string()
    }

    /// Test 1: AUTO compose with a query whose top KG concept is seeded blends
    /// the entity into `data.entities` and the "Knowledge graph" markdown
    /// section, while atoms remain present.
    #[tokio::test]
    async fn auto_compose_blends_seeded_kg_concept() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;
        let concept_id = seed_kg_concept(&f).await;

        let resp = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");

        let data = &resp["data"];
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(!atoms.is_empty(), "atoms must still be present");

        let entities = data["entities"]
            .as_array()
            .expect("entities array must be present when a KG concept blends");
        assert!(
            entities.iter().any(|e| e["id"] == concept_id),
            "expected seeded concept {concept_id} in entities, got: {entities:?}"
        );

        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            md.contains("Knowledge graph"),
            "markdown must contain the Knowledge graph section, got: {md}"
        );
        assert!(
            md.contains("ZipCache"),
            "markdown must render the blended concept's name, got: {md}"
        );
    }

    /// #1505 direct-call exactness: even when the authorized token's primary
    /// namespace matches the explicit arm, a broader visible set must be
    /// narrowed before KG blending. Otherwise a local-only entity leaks into
    /// an arm briefing while corpus rows remain correctly arm-scoped.
    #[tokio::test]
    async fn direct_compose_narrows_matching_broad_token_before_kg_blend() {
        use khive_runtime::PackRuntime;

        let rt = rt_with_marker_embedder();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("kg registry");
        let local_entity = registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "LocalOnlyZipCache",
                    "description": format!(
                        "{MARKER} local-only paged KV cache quantization technique"
                    ),
                }),
            )
            .await
            .expect("create local-only KG entity")["id"]
            .as_str()
            .expect("entity id")
            .to_string();

        let arm_ns = Namespace::parse("bench-arm-a").expect("arm namespace");
        let arm_entity = registry
            .dispatch(
                "create",
                json!({
                    "namespace": arm_ns.as_str(),
                    "kind": "concept",
                    "name": "ArmOnlyZipCache",
                    "description": format!(
                        "{MARKER} arm-only paged KV cache quantization technique"
                    ),
                }),
            )
            .await
            .expect("create arm-only KG entity")["id"]
            .as_str()
            .expect("arm entity id")
            .to_string();
        let broad_arm_token = rt
            .authorize_with_visibility(arm_ns.clone(), vec![Namespace::local()])
            .expect("arm token with local visibility");
        let knowledge = KnowledgePack::new(rt.clone());
        knowledge
            .dispatch(
                "knowledge.upsert_atoms",
                json!({
                    "atoms": [{
                        "slug": "arm-kg-blend-atom",
                        "name": "Arm KG Blend Atom",
                        "content": format!("{MARKER} {OVERLAP_CONTENT}"),
                        "finalized": true,
                    }]
                }),
                &registry,
                &broad_arm_token,
            )
            .await
            .expect("upsert arm atom");
        knowledge
            .dispatch(
                "knowledge.upsert_domains",
                json!({
                    "domains": [{
                        "slug": "arm-kg-blend-domain",
                        "name": "Arm KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["arm-kg-blend-atom"],
                    }]
                }),
                &registry,
                &broad_arm_token,
            )
            .await
            .expect("upsert arm domain");

        let resp = knowledge
            .dispatch(
                "knowledge.compose",
                json!({
                    "namespace": arm_ns.as_str(),
                    "domain_ids": ["arm-kg-blend-domain"],
                    "query": QUERY,
                }),
                &registry,
                &broad_arm_token,
            )
            .await
            .expect("direct exact-arm compose");
        let entities = resp["data"]["entities"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            entities.iter().any(|entity| entity["id"] == arm_entity),
            "the arm KG candidate must prove the blend leg ran: {entities:?}"
        );
        assert!(
            entities.iter().all(|entity| entity["id"] != local_entity),
            "local-only KG entity leaked through a matching but broad direct-call token: {entities:?}"
        );
    }

    /// Test 2: `blend_kg=false` behaves exactly as compose did before this
    /// feature — no `entities` field, no "Knowledge graph" markdown section —
    /// even when a matching KG concept exists.
    #[tokio::test]
    async fn blend_kg_false_omits_entities() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "blend_kg": false }),
            )
            .await
            .expect("compose with blend_kg=false ok");

        let data = &resp["data"];
        assert!(
            data.get("entities").is_none(),
            "entities must be absent when blend_kg=false, got: {data}"
        );
        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            !md.contains("Knowledge graph"),
            "markdown must not contain a Knowledge graph section when blend_kg=false, got: {md}"
        );
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(!atoms.is_empty(), "atoms must still be present");
    }

    /// Test 3: `atom_ids`-only calls never blend, regardless of `blend_kg`
    /// (which defaults to true) — the caller pinned exact atoms.
    #[tokio::test]
    async fn atom_ids_only_never_blends() {
        let f = pack(rt_with_marker_embedder());
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": OVERLAP_CONTENT }
                ]
            }),
        )
        .await
        .expect("upsert atom");
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "atom_ids": ["kg-blend-atom"], "query": QUERY }),
            )
            .await
            .expect("atom_ids-only compose ok");

        let data = &resp["data"];
        assert!(
            data.get("entities").is_none(),
            "atom_ids-only compose must never blend KG entities, got: {data}"
        );
        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            !md.contains("Knowledge graph"),
            "atom_ids-only markdown must not contain a Knowledge graph section, got: {md}"
        );
    }

    /// Test 4: blended entities respect `max_tokens` trimming — a budget too
    /// tight to fit any entity after the atom/section body omits the
    /// "Knowledge graph" section entirely, while the atom body itself
    /// survives untouched.
    #[tokio::test]
    async fn blended_entities_respect_max_tokens_budget() {
        let f = pack(rt_with_marker_embedder());

        // A padded atom whose cost alone consumes nearly all of the
        // minimum-clamped max_tokens=500 budget (2000 chars), leaving too
        // little remaining for the entity section but not exceeding the
        // budget itself — the atom must survive either way.
        let filler = "x".repeat(1677);
        let big_content = format!("{OVERLAP_CONTENT} {filler}");
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": big_content, "finalized": true }
                ]
            }),
        )
        .await
        .expect("upsert atom");
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "kg-blend-domain",
                        "name": "KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["kg-blend-atom"]
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain");
        seed_kg_concept(&f).await;

        // Generous budget: entity fits.
        let generous = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "max_tokens": 8000 }),
            )
            .await
            .expect("generous-budget compose ok");
        let generous_atoms = generous["data"]["atoms"].as_array().expect("atoms array");
        assert!(!generous_atoms.is_empty(), "atoms must be present");
        assert!(
            generous["data"].get("entities").is_some(),
            "entity must fit under a generous budget, got: {}",
            generous["data"]
        );

        // Minimum-clamp budget: the atom body alone consumes it, leaving no
        // room for the entity section — atoms must still survive.
        let tight = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "max_tokens": 500 }),
            )
            .await
            .expect("tight-budget compose ok");
        let tight_atoms = tight["data"]["atoms"].as_array().expect("atoms array");
        assert!(!tight_atoms.is_empty(), "atoms must survive a tight budget");
        assert!(
            tight["data"].get("entities").is_none(),
            "entity must be trimmed out under a tight budget, got: {}",
            tight["data"]
        );
    }

    async fn seed_kg_document(f: &Fixture) -> String {
        let resp = f
            .dispatch(
                "create",
                json!({
                    "kind": "document",
                    "name": "ADR-051",
                    "description": format!(
                        "{MARKER} section-embeddings hybrid compose architecture decision record"
                    ),
                }),
            )
            .await
            .expect("create kg document");
        resp["id"].as_str().expect("entity id").to_string()
    }

    /// Test 5: explicit `domain_ids` (non-AUTO) calls blend exactly like AUTO
    /// calls do — the Amendment 1 decision covers both, not just AUTO.
    #[tokio::test]
    async fn explicit_domain_ids_compose_blends_seeded_kg_concept() {
        let f = pack(rt_with_marker_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        let concept_id = seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": QUERY }),
            )
            .await
            .expect("explicit domain_ids compose ok");

        let data = &resp["data"];
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(!atoms.is_empty(), "atoms must still be present");
        let entities = data["entities"]
            .as_array()
            .expect("entities array must be present for explicit domain_ids blend");
        assert!(
            entities.iter().any(|e| e["id"] == concept_id),
            "expected seeded concept {concept_id} in entities, got: {entities:?}"
        );
    }

    /// Test 6: a `document`-kind KG entity (not just `concept`) blends.
    #[tokio::test]
    async fn auto_compose_blends_seeded_kg_document() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;
        let document_id = seed_kg_document(&f).await;

        let resp = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");

        let data = &resp["data"];
        let entities = data["entities"]
            .as_array()
            .expect("entities array must be present when a KG document blends");
        assert!(
            entities
                .iter()
                .any(|e| e["id"] == document_id && e["kind"] == "document"),
            "expected seeded document {document_id} in entities, got: {entities:?}"
        );
    }

    /// Test 7: the blended candidate pool is capped at `KG_BLEND_CAP` (5) and
    /// deduplicated — seeding more matching concept/document entities than
    /// the cap never yields duplicate or over-cap results.
    #[tokio::test]
    async fn blend_candidates_are_capped_and_deduped_across_kinds() {
        let f = pack(rt_with_marker_embedder());
        seed_domain_and_atom(&f).await;

        for i in 0..4 {
            f.dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": format!("Concept{i}"),
                    "description": format!("{MARKER} paged KV cache technique variant {i}"),
                }),
            )
            .await
            .expect("create kg concept");
        }
        for i in 0..4 {
            f.dispatch(
                "create",
                json!({
                    "kind": "document",
                    "name": format!("Document{i}"),
                    "description": format!("{MARKER} decode-time attention paper variant {i}"),
                }),
            )
            .await
            .expect("create kg document");
        }

        let resp = f
            .dispatch("knowledge.compose", json!({ "query": QUERY }))
            .await
            .expect("auto compose ok");

        let entities = resp["data"]["entities"]
            .as_array()
            .expect("entities array must be present — 8 matching entities were seeded");
        assert!(
            entities.len() <= 5,
            "blended entities must be capped at KG_BLEND_CAP=5, got {}: {entities:?}",
            entities.len()
        );
        let ids: HashSet<&str> = entities.iter().filter_map(|e| e["id"].as_str()).collect();
        assert_eq!(
            ids.len(),
            entities.len(),
            "blended entities must be deduplicated by id, got: {entities:?}"
        );
    }

    /// Test 8: zero-atom edge case (ADR-051 Amendment 1 Decision) — when the
    /// final compose body ends up with zero atoms (everything trimmed by
    /// `max_tokens`), the entity inclusion floor is undefined, so no
    /// entities blend even though the leftover budget would easily fit one.
    #[tokio::test]
    async fn zero_atoms_in_final_body_blends_no_entities_even_with_budget_room() {
        let f = pack(rt_with_marker_embedder());

        // Oversized atom: alone it exceeds the max_tokens=500 (2000-char)
        // budget, so it is excluded from the body entirely — body_used stays
        // 0, leaving the *entire* budget as "remaining" for the entity trim.
        let filler = "x".repeat(2500);
        let big_content = format!("{OVERLAP_CONTENT} {filler}");
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "kg-blend-atom", "name": "KG Blend Atom", "content": big_content, "finalized": true }
                ]
            }),
        )
        .await
        .expect("upsert atom");
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": "kg-blend-domain",
                        "name": "KG Blend Domain",
                        "description": OVERLAP_CONTENT,
                        "members": ["kg-blend-atom"]
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain");
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "query": QUERY, "max_tokens": 500 }),
            )
            .await
            .expect("compose ok");

        let data = &resp["data"];
        assert!(
            data.get("entities").is_none(),
            "zero-atom final body must blend no entities even with room in the budget, got: {data}"
        );
    }

    // ── degrade-not-abort ───────────────────────────────────────────────────

    /// Embedder that fails exactly the single-text `embed_query` calls the KG
    /// blend path's `hybrid_search` → `vector_search` makes for the literal
    /// compose query text, while behaving like `MarkerEmbedService` for every
    /// other call (batched atom rerank, entity setup during seeding). This
    /// isolates the failure to the blend boundary without breaking fixture
    /// setup or the atom-scoring path, which only ever calls `embed` with
    /// batches of 2+ texts (query + candidates).
    struct FailingBlendEmbedService;

    const CANCEL_QUERY: &str = "zzzquantumfoo cancellation boundary kv cache paging decode \
        attention retrieval augmented generation dense sparse benchmark corpus";

    static CANCEL_ON_BLEND_FAILURE: std::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>> =
        std::sync::Mutex::new(None);

    #[async_trait]
    impl EmbeddingService for FailingBlendEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            if texts.len() == 1 && (texts[0] == QUERY || texts[0] == CANCEL_QUERY) {
                if texts[0] == CANCEL_QUERY {
                    if let Some(cancel) = CANCEL_ON_BLEND_FAILURE
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                    {
                        let _ = cancel.send(true);
                    }
                }
                return Err(EmbedError::Internal(
                    "simulated KG blend-path embed failure".into(),
                ));
            }
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; DIM];
                    if t.contains(MARKER) {
                        v[0] = 1.0;
                    } else {
                        v[1] = 1.0;
                    }
                    v
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "failing-blend-embed-service"
        }
    }

    struct FailingBlendEmbedProvider;

    #[async_trait]
    impl EmbedderProvider for FailingBlendEmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(FailingBlendEmbedService))
        }
    }

    fn rt_with_failing_blend_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            brain_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        rt.register_embedder(FailingBlendEmbedProvider);
        rt
    }

    /// Test 9 (High finding regression): a KG-blend-path embed failure
    /// (`hybrid_search` → `vector_search` → `embed_query` erroring) must not
    /// abort `knowledge.compose` — it must degrade to the already-finalized
    /// atom-only response. Uses explicit `domain_ids` (not AUTO) so the
    /// `knowledge.suggest` phase never calls `embed_query` on `QUERY` itself,
    /// keeping the induced failure isolated to the KG blend call.
    #[tokio::test]
    async fn kg_blend_failure_degrades_to_atom_only_response() {
        let f = pack(rt_with_failing_blend_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": QUERY }),
            )
            .await
            .expect("compose must succeed atom-only despite KG blend failure");

        let data = &resp["data"];
        let atoms = data["atoms"].as_array().expect("atoms array");
        assert!(
            !atoms.is_empty(),
            "atom-only body must survive a KG blend failure, got: {data}"
        );
        assert!(
            data.get("entities").is_none(),
            "entities must be absent when the KG blend fails, got: {data}"
        );
        let md = data["markdown"].as_str().expect("markdown");
        assert!(
            !md.contains("Knowledge graph"),
            "markdown must not contain a Knowledge graph section when the blend fails, got: {md}"
        );
    }

    #[tokio::test]
    async fn cancellation_during_kg_blend_failure_never_degrades_to_success() {
        let f = pack(rt_with_failing_blend_embedder());
        let domain_id = seed_domain_and_atom(&f).await;
        seed_kg_concept(&f).await;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        *CANCEL_ON_BLEND_FAILURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel_tx);

        let result = khive_storage::scope_request_read_cancellation(
            cancel_rx,
            f.dispatch(
                "knowledge.compose",
                json!({ "domain_ids": [domain_id], "query": CANCEL_QUERY }),
            ),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(khive_runtime::RuntimeError::Storage(
                    khive_storage::StorageError::Timeout { .. }
                ))
            ),
            "request cancellation at the KG-blend catch must propagate, got {result:?}"
        );
    }
}
