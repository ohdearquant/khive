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

    async fn dispatch_ns(
        &self,
        verb: &str,
        ns: &str,
        mut args: Value,
    ) -> Result<Value, RuntimeError> {
        args["namespace"] = json!(ns);
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

    f.dispatch_ns(
        "knowledge.upsert_atoms",
        "ns-a",
        json!({ "atoms": [{ "slug": "shared-slug", "name": "NSA", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert ns-a");

    f.dispatch_ns(
        "knowledge.edit",
        "ns-a",
        json!({ "id": "shared-slug", "sections": [{ "section_type": "overview", "content": "This section belongs exclusively to namespace A and must not be visible from namespace B under any circumstances." }] }),
    )
    .await
    .expect("edit ns-a");

    f.dispatch_ns(
        "knowledge.upsert_atoms",
        "ns-b",
        json!({ "atoms": [{ "slug": "shared-slug", "name": "NSB", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert ns-b");

    let got_b = f
        .dispatch_ns(
            "knowledge.get",
            "ns-b",
            json!({ "id": "shared-slug", "include_sections": true }),
        )
        .await
        .expect("get ns-b");

    let sections_b = got_b["sections"].as_array().expect("sections array");
    assert_eq!(sections_b.len(), 0, "ns-b atom must not see ns-a sections");
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
            { "slug": "rag",             "name": "RAG",               "content": "Retrieval-Augmented Generation combines retrieval with generation — covering concepts techniques algorithms implementations applications use cases and design patterns in detail RAG retrieves relevant passages before generating text dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "tags": ["retrieval", "rag"] },
            { "slug": "lora",            "name": "LoRA",              "content": "Low-Rank Adaptation of large language models — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["fine-tuning", "adapter"] },
            { "slug": "flash-attention", "name": "FlashAttention",    "content": "Memory-efficient attention using tiling — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering", "tags": ["attention", "gpu"] },
            { "slug": "gqa",             "name": "GQA",               "content": "Grouped Query Attention reduces KV cache — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["attention", "inference"] },
            { "slug": "rope",            "name": "RoPE",              "content": "Rotary Position Embedding for transformers — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —", "tags": ["embedding", "position"] },
            { "slug": "agent",           "name": "Agent",             "content": "Autonomous agent using LLM tool calls — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["agent", "tool-use"] },
            { "slug": "chain-of-thought","name": "Chain-of-Thought",  "content": "Prompting technique for step-by-step reasoning — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —", "tags": ["reasoning", "prompting"] },
            { "slug": "speculative",     "name": "Speculative Decoding", "content": "Draft model accelerates inference via speculation — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["inference", "draft"] },
            { "slug": "quantization",    "name": "Quantization",     "content": "Reduce model size by lowering numerical precision — covering concepts techniques algorithms implementations applications use cases and design patterns in", "tags": ["compression", "inference"] },
            { "slug": "dpo",             "name": "DPO",               "content": "Direct Preference Optimization for RLHF alignment — covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "tags": ["fine-tuning", "alignment"] },
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
