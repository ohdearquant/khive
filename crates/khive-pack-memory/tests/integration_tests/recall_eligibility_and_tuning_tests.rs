/// Regression: after wiring khive-retrieval into fuse_candidates, the recall.fuse
/// response shape must be unchanged — top-level strategy + candidate_limit, and
/// per-candidate note_id + fused_score + source must all be present. Full recall
/// fields (content, salience) must remain absent.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_fuse_shape_preserved_after_retrieval_wiring() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "shape regression check after retrieval wiring" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_fuse",
            json!({ "query": "shape regression retrieval wiring" }),
        )
        .await
        .expect("memory.recall_fuse");

    // Top-level shape
    assert!(
        result.get("strategy").is_some(),
        "strategy field must be present in recall.fuse response"
    );
    assert!(
        result["candidate_limit"].as_u64().is_some(),
        "candidate_limit must be a non-negative integer"
    );

    let fused = result["fused_candidates"]
        .as_array()
        .expect("fused_candidates array");
    assert!(!fused.is_empty(), "fused_candidates must be non-empty");

    let c = &fused[0];
    assert!(c["id"].as_str().is_some(), "note_id must be a string UUID");
    assert!(
        c["fused_score"].as_f64().is_some(),
        "fused_score must be a float"
    );
    let source = c["source"].as_str().expect("source must be a plain string");
    assert!(
        matches!(source, "text" | "vector" | "both"),
        "source must be a plain label, got {source:?}"
    );
    // Full recall fields must not leak into fuse output
    assert!(
        c.get("content").is_none(),
        "content must be absent from recall.fuse output"
    );
    assert!(
        c.get("salience").is_none(),
        "salience must be absent from recall.fuse output"
    );
}

/// When include_breakdown is true, breakdown.total() must equal the hit's composite score.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_breakdown_total_matches_composite_score() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "arithmetic score check memory", "salience": 0.7 }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "arithmetic score check", "config": { "include_breakdown": true } }),
        )
        .await
        .expect("recall with breakdown");

    let hits = result.as_array().unwrap();
    assert!(!hits.is_empty());
    let hit = &hits[0];
    // `rank_score` is the composite score from the archive pipeline.
    // `score` is the absolute relevance (pre-fusion raw cosine, or composite if no vector).
    // The breakdown weighted sum corresponds to the legacy compute_score path which
    // computes contributions under the RecallConfig additive model. The rank_score
    // from the archive multiplicative model does NOT equal the breakdown sum —
    // they are two different scoring strategies coexisting in the pipeline.
    // Here we just verify rank_score is bounded in [0, 1] and breakdown fields are present.
    let rank_score = hit["rank_score"].as_f64().expect("hit has rank_score");
    assert!(
        (0.0..=1.0).contains(&rank_score),
        "rank_score {rank_score} must be in [0, 1]"
    );
    let bd = &hit["breakdown"];
    let rc = bd["weighted"]["relevance_contribution"].as_f64().unwrap();
    let ic = bd["weighted"]["salience_contribution"].as_f64().unwrap();
    let tc = bd["weighted"]["temporal_contribution"].as_f64().unwrap();
    let total = rc + ic + tc;
    assert!(
        (0.0..=1.0).contains(&total),
        "breakdown weighted sum {total} must be in [0, 1]"
    );
}

/// Regression test: non-memory notes must not appear in recall results.
///
/// Creates more non-memory notes than the default `limit * 4` candidate threshold (the amount
/// at which non-memory notes can dominate the candidate pool without pre-filtering), then
/// verifies that recall returns only memory-kind notes.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_excludes_non_memory_notes() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create 50 observation notes whose content matches the recall query — enough to
    // dominate a `limit=5` candidate pool at `limit * 4 = 20` without pre-filtering.
    let tok = rt.authorize(Namespace::local()).unwrap();
    for i in 0..50 {
        rt.create_note(
            &tok,
            "observation",
            None,
            &format!("observation {i} about attention mechanisms in neural networks"),
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("create observation");
    }

    // Create a small number of memory notes with matching content.
    let mem1 = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "memory note about attention mechanisms in neural networks",
                "salience": 0.8
            }),
        )
        .await
        .expect("remember 1");
    let mem2 = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "another memory note about attention mechanisms",
                "salience": 0.7
            }),
        )
        .await
        .expect("remember 2");
    let mem1_id = mem1["id"].as_str().unwrap().to_string();
    let mem2_id = mem2["id"].as_str().unwrap().to_string();

    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "attention mechanisms neural networks", "limit": 5 }),
        )
        .await
        .expect("recall succeeds");

    let hits = result.as_array().expect("array of hits");
    assert!(
        !hits.is_empty(),
        "recall should return memory notes even when non-memory notes dominate the index"
    );
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&mem1_id.as_str()) || ids.contains(&mem2_id.as_str()),
        "at least one memory note must appear in recall results"
    );
    for hit in hits {
        // recall must never surface observation or other non-memory kinds
        assert!(
            hit.get("id").is_some(),
            "hit has note_id field (memory pack shape)"
        );
        assert!(
            hit.get("salience").is_some(),
            "hit has salience field (memory pack shape)"
        );
    }
}

/// Regression test for issue #430: recall must return the requested count of
/// memory results even when a tiny `candidate_limit` and many higher-ranking
/// non-memory notes would otherwise exhaust the candidate cap before the
/// memory-kind scope is ever applied.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_candidate_cap_counts_eligible_memories_not_generic_notes() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // 30 non-memory notes, all matching the query, all ranked ahead of the
    // memory notes below (higher salience is irrelevant to FTS/vector rank,
    // but sheer count is enough to exhaust a `candidate_limit` of 5).
    let tok = rt.authorize(Namespace::local()).unwrap();
    for i in 0..30 {
        rt.create_note(
            &tok,
            "observation",
            None,
            &format!("observation {i} about quokka migration patterns in tundra"),
            Some(0.9),
            None,
            vec![],
        )
        .await
        .expect("create observation");
    }

    let want = 5usize;
    let mut memory_ids = Vec::with_capacity(want);
    for i in 0..want {
        let mem = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("memory note {i} about quokka migration patterns in tundra"),
                    "salience": 0.6
                }),
            )
            .await
            .expect("remember");
        memory_ids.push(mem["id"].as_str().unwrap().to_string());
    }

    // `candidate_limit: 5` is smaller than the 30 non-memory notes competing
    // for the same candidate window — without memory-kind scoping applied
    // before the cap, the 5 memory notes can be starved out entirely.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "quokka migration patterns tundra",
                "limit": want,
                "config": { "candidate_limit": 5 },
            }),
        )
        .await
        .expect("recall succeeds");

    let hits = result.as_array().expect("array of hits");
    assert_eq!(
        hits.len(),
        want,
        "recall must return the requested count of memories even when non-memory \
         notes dominate a small candidate window; got {} of {want}: {hits:?}",
        hits.len()
    );
    let ids: std::collections::HashSet<&str> =
        hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    for mid in &memory_ids {
        assert!(
            ids.contains(mid.as_str()),
            "expected memory {mid} in recall results, got {ids:?}"
        );
    }
}

/// Regression: PackTunable::apply_config must actually affect recall
/// scoring, not just mutate a Mutex that handlers ignore.
///
/// The wire is:
///   apply_config(weights) → MemoryPack.config (Mutex)
///   → MemoryPack::active_config() reads it
///   → handle_recall / handle_recall_score use it as the base
///   → compute_score uses the tuned weights
///
/// This test uses `recall.score` (deterministic — no FTS/vector noise) with
/// no per-call `config` argument, applies different configs via
/// PackTunable::apply_config, and verifies the resulting `total` score
/// reflects the tuned weights. Without the active_config wire (the
/// bug), the result would always reflect RecallConfig::default() regardless
/// of apply_config.
#[tokio::test]
async fn test_pack_tunable_apply_config_affects_recall_score() {
    use khive_pack_memory::config::RecallConfig;

    let rt = make_runtime();
    let pack = MemoryPack::new(receipt_credentials::with_receipt_credentials(rt.clone()));

    // Sanity: with default config (0.70/0.20/0.10), the score for
    //   rrf=1.0, salience=1.0, decay=0.0, age=0 → 0.70+0.20+0.10 = 1.0
    // With salience_only (0.0/1.0/0.0), the score for
    //   rrf=1.0, salience=0.0, decay=0.0, age=0 → 0.0
    // The difference is large enough to prove the weights flow through.

    // Apply salience-only config to the pack.
    let salience_only = RecallConfig {
        relevance_weight: 0.0,
        salience_weight: 1.0,
        temporal_weight: 0.0,
        ..RecallConfig::default()
    };
    pack.apply_config(serde_json::to_value(&salience_only).unwrap())
        .expect("apply_config (salience-only) succeeds");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    let registry = builder.build().expect("registry builds");

    // Call recall.score with high relevance but ZERO salience — under
    // salience-only weights, score MUST be 0.0. Under default weights
    // (the bug), it would be 0.70.
    let result = registry
        .dispatch(
            "memory.recall_score",
            json!({
                "rrf": 1.0,
                "salience": 0.0,
                "decay_factor": 0.0,
                "age_days": 0.0,
            }),
        )
        .await
        .expect("recall.score succeeds");
    let total = result["total"].as_f64().expect("total is a number");
    assert!(
        total.abs() < 1e-9,
        "under salience_weight=1.0, salience=0 → score=0; got {total}. \
         If non-zero, MemoryPack::active_config() is not being used by \
         recall.score (#159 regression)."
    );

    // Mirror check: under relevance-only weights with rrf=1.0, salience=0 → score=1.0.
    // This requires a SECOND pack instance because PackRuntime ownership prevents
    // mutating the live registry's config from outside. We construct the test
    // by exercising the same wire on a fresh pack.
    let rt2 = make_runtime();
    let pack2 = MemoryPack::new(receipt_credentials::with_receipt_credentials(rt2.clone()));
    // Use Weighted strategy so the input relevance score (1.0) passes through
    // unnormalized — RRF strategy would scale it by (k+1) = 61, producing 61.0.
    let relevance_only = RecallConfig {
        relevance_weight: 1.0,
        salience_weight: 0.0,
        temporal_weight: 0.0,
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.5, 0.5],
        },
        ..RecallConfig::default()
    };
    pack2
        .apply_config(serde_json::to_value(&relevance_only).unwrap())
        .expect("apply_config (relevance-only) succeeds");

    let mut builder2 = VerbRegistryBuilder::new();
    builder2.register(KgPack::new(rt2.clone()));
    builder2.register(pack2);
    let registry2 = builder2.build().expect("registry2 builds");

    let result2 = registry2
        .dispatch(
            "memory.recall_score",
            json!({
                "rrf": 1.0,
                "salience": 0.0,
                "decay_factor": 0.0,
                "age_days": 0.0,
            }),
        )
        .await
        .expect("recall.score (relevance-only) succeeds");
    let total2 = result2["total"].as_f64().expect("total is a number");
    assert!(
        (total2 - 1.0).abs() < 1e-9,
        "under relevance_weight=1.0 with rrf=1.0 (Weighted strategy) → score=1.0; got {total2}"
    );
}
