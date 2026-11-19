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
async fn search_body_lines_distinguishes_filled_atom_from_stub() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "body-lines-filled",
                    "name": "Body Lines Filled",
                    "content": "coveragebodysignal filled description covering concepts techniques algorithms implementations applications use cases design patterns retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline",
                    "finalized": true
                },
                {
                    "slug": "body-lines-stub",
                    "name": "Body Lines Stub",
                    "content": "coveragebodysignal stub description covering concepts techniques algorithms implementations applications use cases design patterns retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline",
                    "finalized": true
                },
                {
                    "slug": "body-lines-newline",
                    "name": "Body Lines Newline",
                    "content": "coveragebodysignal newline convention atom covering terminal newline and blank interior line counting for the aggregate body size signal across stored sections in search results",
                    "finalized": true
                }
            ]
        }),
    )
    .await
    .expect("seed matching atoms");

    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "body-lines-filled",
            "sections": [
                {
                    "section_type": "overview",
                    "content": "First overview line explains the filled atom with enough detail for section validation.\nSecond overview line adds retrieval, indexing, and ranking details for the body.\nThird overview line completes the substantive atom body used by coverage tooling."
                },
                {
                    "section_type": "examples",
                    "content": "First example line demonstrates a realistic filled knowledge atom for callers.\nSecond example line provides another concrete body line for the size signal."
                }
            ]
        }),
    )
    .await
    .expect("add filled atom sections");

    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "body-lines-newline",
            "sections": [
                {
                    "section_type": "overview",
                    "content": "First convention line before a deliberately blank interior line in this section.\n\nThird visible line ends the section with a terminal newline that must not count.\n"
                }
            ]
        }),
    )
    .await
    .expect("add newline-convention atom section");

    let response = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "coveragebodysignal", "rerank": false }),
        )
        .await
        .expect("search matching atoms");
    let results = response["results"].as_array().expect("results array");
    let filled = results
        .iter()
        .find(|result| result["slug"] == "body-lines-filled")
        .expect("filled atom result");
    let stub = results
        .iter()
        .find(|result| result["slug"] == "body-lines-stub")
        .expect("stub atom result");

    assert_eq!(
        filled["body_lines"].as_u64(),
        Some(5),
        "filled atom must expose its aggregate section line count: {filled}"
    );
    assert_eq!(
        stub["body_lines"].as_u64(),
        Some(0),
        "sectionless atom must expose a zero body line count: {stub}"
    );

    let newline = results
        .iter()
        .find(|result| result["slug"] == "body-lines-newline")
        .expect("newline-convention atom result");
    assert_eq!(
        newline["body_lines"].as_u64(),
        Some(3),
        "str::lines() convention: a terminal newline adds no line and a blank \
         interior line counts (line, blank, line = 3): {newline}"
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
