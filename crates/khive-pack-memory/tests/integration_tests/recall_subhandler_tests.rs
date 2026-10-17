/// ADR-033 §2 (F222): recall.rerank is callable and returns expected shape.
#[tokio::test]
async fn test_recall_rerank_passthrough_with_no_active_rerankers() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let candidates = json!([
        { "id": "00000000-0000-0000-0000-000000000001", "fused_score": 0.8 },
        { "id": "00000000-0000-0000-0000-000000000002", "fused_score": 0.6 },
    ]);

    let result = registry
        .dispatch("memory.recall_rerank", json!({ "candidates": candidates }))
        .await
        .expect("recall.rerank with no active rerankers");

    let reranked = result["reranked"].as_array().expect("reranked array");
    assert_eq!(reranked.len(), 2, "must return one entry per candidate");
    for entry in reranked {
        let scores = entry["rerank_scores"]
            .as_object()
            .expect("rerank_scores object");
        assert!(
            scores.is_empty(),
            "no active rerankers → empty rerank_scores, got {scores:?}"
        );
    }
    let active = result["active_rerankers"]
        .as_array()
        .expect("active_rerankers array");
    assert!(active.is_empty(), "no active rerankers expected");
}

#[test]
fn test_memory_dotted_verbs_registered() {
    let names: Vec<&str> = MemoryPack::HANDLERS.iter().map(|v| v.name).collect();
    assert!(names.contains(&"memory.recall_candidates"));
    assert!(names.contains(&"memory.recall_fuse"));
    assert!(names.contains(&"memory.recall_score"));
    assert!(names.contains(&"memory.recall_embed"));
    // F222: recall.rerank must be registered (ADR-033 §2)
    assert!(
        names.contains(&"memory.recall_rerank"),
        "recall.rerank not found in: {names:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_candidates_returns_arrays() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "attention recall candidates" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_candidates",
            json!({ "query": "attention candidates" }),
        )
        .await
        .expect("memory.recall_candidates");

    let text = result["text_candidates"].as_array().expect("text array");
    assert!(!text.is_empty());
    assert!(text[0]["id"].as_str().is_some());
    assert!(text[0]["score"].as_f64().is_some());
    assert!(text[0]["rank"].as_u64().is_some());
    assert!(result["candidate_limit"].as_u64().is_some());
    assert!(
        result.get("text_hits").is_none(),
        "old count field must be absent"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_fuse_returns_fused_candidates_not_full_recall() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "attention fusion diagnostic" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch("memory.recall_fuse", json!({ "query": "attention fusion" }))
        .await
        .expect("memory.recall_fuse");

    let fused = result["fused_candidates"].as_array().expect("fused array");
    assert!(!fused.is_empty());
    assert!(fused[0]["fused_score"].as_f64().is_some());
    assert!(fused[0]["source"].as_str().is_some());
    assert!(
        fused[0].get("content").is_none(),
        "full recall field must be absent"
    );
    assert!(
        fused[0].get("salience").is_none(),
        "full recall field must be absent"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_breakdown_is_opt_in() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "attention score breakdown", "salience": 0.8 }),
        )
        .await
        .expect("memory.remember");

    let plain = registry
        .dispatch("memory.recall", json!({ "query": "attention breakdown" }))
        .await
        .expect("memory.recall");
    let hits = plain.as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits[0].get("breakdown").is_none(),
        "breakdown must be absent by default"
    );

    let explained = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "attention breakdown", "config": { "include_breakdown": true } }),
        )
        .await
        .expect("recall with breakdown");
    let hits = explained.as_array().unwrap();
    assert!(!hits.is_empty());
    let bd = &hits[0]["breakdown"];
    assert!(bd["relevance"].as_f64().is_some());
    assert!(bd["salience_raw"].as_f64().is_some());
    assert!(bd["salience_decayed"].as_f64().is_some());
    assert!(bd["temporal"].as_f64().is_some());
    assert!(bd["weighted"]["relevance_contribution"].as_f64().is_some());
}

/// recall.candidates always includes both array keys even when the embedding model is absent
/// and the vector path returns nothing.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_candidates_vector_field_always_present() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "text only candidate check" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_candidates",
            json!({ "query": "text only candidate" }),
        )
        .await
        .expect("memory.recall_candidates");

    // Both arrays must be present even if one is empty.
    assert!(
        result["vector_candidates"].as_array().is_some(),
        "vector_candidates key must always be present"
    );
    assert!(
        result["text_candidates"].as_array().is_some(),
        "text_candidates key must always be present"
    );
}

/// recall.fuse source field must be a plain string ("text"), not a serde-tagged enum.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_fuse_source_field_is_plain_string() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "fuse source string check" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_fuse",
            json!({ "query": "fuse source string" }),
        )
        .await
        .expect("memory.recall_fuse");

    let fused = result["fused_candidates"].as_array().expect("fused array");
    assert!(!fused.is_empty());
    let source = fused[0]["source"].as_str().expect("source is string");
    // Must be a plain label, not a JSON object or enum tag.
    assert!(
        source == "text" || source == "vector" || source == "both",
        "source must be a plain label, got {source:?}"
    );
}

/// Verifies that recall.fuse routes through khive_retrieval::fuse_search_results
/// by injecting a non-default fusion config (Rrf k=1) and asserting the fused
/// score matches the RRF k=1 formula: 1/(k + rank) = 1/(1 + 1) = 0.5.
///
/// Under default k=60 the score would be 1/61 ≈ 0.0164. The large gap (0.5 vs
/// 0.0164) is the discriminator: if the adapter did not pass k=1 through to
/// khive_retrieval::HybridConfig, the score would not be 0.5.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_fuse_rrf_k1_uses_retrieval_adapter() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "retrieval adapter rrf k1 probe memory" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_fuse",
            json!({
                "query": "retrieval adapter rrf k1 probe",
                "config": {
                    "fuse_strategy": { "rrf": { "k": 1 } }
                }
            }),
        )
        .await
        .expect("recall.fuse with Rrf k=1");

    let fused = result["fused_candidates"].as_array().expect("fused array");
    assert!(
        !fused.is_empty(),
        "recall.fuse must return at least one candidate"
    );

    let score = fused[0]["fused_score"]
        .as_f64()
        .expect("fused_score is f64");
    // Rank 1 in a single text source with k=1: RRF = 1/(1+1) = 0.5.
    // If k=60 were used instead, score ≈ 0.0164 — the gap proves the adapter works.
    let expected = 0.5_f64;
    assert!(
        (score - expected).abs() < 1e-6,
        "RRF k=1, rank 1 → fused_score must be 0.5; got {score:.6} \
         (≈0.0164 means the adapter passed k=60 instead of k=1)"
    );
}

/// A recall request that names `rrf` fuses with the configured constant instead of a literal 60.
///
/// Three memories that all match the query rank first, second and third in the text arm. RRF
/// relevance is calibrated by position between the best and the worst fused score, so the
/// middle hit's relevance depends on the constant while the other two do not.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_request_naming_rrf_uses_the_configured_constant() {
    const QUERY: &str = "rrf constant probe vector search";

    async fn middle_rank_score(registry: &VerbRegistry, params: Value) -> f64 {
        let result = registry
            .dispatch("memory.recall", params)
            .await
            .expect("memory.recall succeeds");
        let hit_list = result.as_array().expect("array of hits");
        assert_eq!(
            hit_list.len(),
            3,
            "all three memories must be recalled: {hit_list:?}"
        );
        let scores: Vec<f64> = hit_list
            .iter()
            .map(|h| h["rank_score"].as_f64().expect("rank_score"))
            .collect();
        let total: f64 = scores.iter().sum();
        let lowest = scores.iter().copied().fold(f64::INFINITY, f64::min);
        let highest = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        total - lowest - highest
    }

    let registry = make_registry(make_runtime());
    for content in [
        "rrf constant probe memory about vector search",
        "rrf constant probe record about vector search",
        "rrf constant probe entry about vector search",
    ] {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "salience": 0.7,
                    "decay": 0.01
                }),
            )
            .await
            .expect("memory.remember succeeds");
    }

    let unnamed_params = json!({ "query": QUERY });
    let named_params = json!({ "query": QUERY, "fusion_strategy": "rrf" });
    let named_at_sixty_params = json!({
        "query": QUERY,
        "fusion_strategy": "rrf",
        "config": { "fuse_strategy": { "rrf": { "k": 60 } } }
    });
    let unnamed = middle_rank_score(&registry, unnamed_params).await;
    let named = middle_rank_score(&registry, named_params).await;
    let named_at_sixty = middle_rank_score(&registry, named_at_sixty_params).await;

    assert!(
        named_at_sixty > named + 1e-3,
        "the middle hit must score higher at a configured constant of 60 than at 10: \
         k=10 gave {named}, k=60 gave {named_at_sixty}"
    );
    assert!(
        (named - unnamed).abs() < 1e-4,
        "naming rrf under the default configuration must match omitting the strategy: \
         named {named}, omitted {unnamed}"
    );
}
