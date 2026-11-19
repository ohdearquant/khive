// ── ADR-033 §6 knob tests ──────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_default_identity() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create multiple memories so the identity comparison is meaningful
    // (single-hit fixtures can't distinguish ordering changes).
    for content in [
        "the mitochondria is the powerhouse of the cell",
        "ribosomes synthesize proteins in the cell",
        "the nucleus contains the cell's DNA",
        "lysosomes digest cellular waste in the cell",
    ] {
        registry
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": 0.8 }),
            )
            .await
            .expect("remember succeeds");
    }

    // Baseline recall with no knobs
    let base = registry
        .dispatch("memory.recall", json!({ "query": "cell" }))
        .await
        .expect("baseline recall succeeds");
    let base_hits = base.as_array().expect("array");
    assert!(
        base_hits.len() >= 2,
        "baseline must return at least two hits to make ordering meaningful, got {}",
        base_hits.len()
    );

    // Same call with all three knobs explicitly set to null — must be byte-identical
    let knobless = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "cell",
                "top_k": null,
                "fusion_strategy": null,
                "score_floor": null,
            }),
        )
        .await
        .expect("recall with all knobs null succeeds");
    let knobless_hits = knobless.as_array().expect("array");

    assert_eq!(
        base_hits.len(),
        knobless_hits.len(),
        "null knobs must not change result count"
    );

    // Full ordering identity: each hit's note_id AND fused_score must match
    // position-by-position. This catches a regression where a null knob silently
    // shifts the ranking or rescaling.
    for (i, (b, k)) in base_hits.iter().zip(knobless_hits.iter()).enumerate() {
        assert_eq!(
            b["id"].as_str(),
            k["id"].as_str(),
            "null knobs altered note_id at position {i}"
        );
        // Scores must round-trip; allow tiny float jitter
        let bs = b["score"].as_f64().unwrap_or(0.0);
        let ks = k["score"].as_f64().unwrap_or(0.0);
        assert!(
            (bs - ks).abs() < 1e-9,
            "null knobs altered score at position {i}: baseline={bs} knobless={ks}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_top_k_override() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create several distinct memories to ensure the pool is large enough
    for i in 0..5 {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("rust ownership memory safety concept {i}"),
                    "salience": 0.7
                }),
            )
            .await
            .expect("remember succeeds");
    }

    // Recall with top_k=2 — must not return more than 2 results
    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "rust ownership memory safety", "top_k": 2 }),
        )
        .await
        .expect("recall with top_k=2 succeeds");
    let hits = result.as_array().expect("array");
    assert!(
        hits.len() <= 2,
        "top_k=2 must return at most 2 results, got {}",
        hits.len()
    );

    // top_k=1 must return at most 1
    let result1 = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "rust ownership memory safety", "top_k": 1 }),
        )
        .await
        .expect("recall with top_k=1 succeeds");
    let hits1 = result1.as_array().expect("array");
    assert!(
        hits1.len() <= 1,
        "top_k=1 must return at most 1 result, got {}",
        hits1.len()
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_fusion_strategy_override() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "gradient descent optimization machine learning",
                "salience": 0.8
            }),
        )
        .await
        .expect("remember succeeds");

    // Each valid strategy must succeed and return an array
    for strategy in &["rrf", "weighted", "union", "vector_only", "keyword_only"] {
        let result = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": "gradient descent optimization",
                    "fusion_strategy": strategy
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("recall with fusion_strategy={strategy:?} failed: {e}"));
        assert!(
            result.is_array(),
            "fusion_strategy={strategy:?} must return an array, got {result}"
        );
    }

    // Invalid strategy must return an error
    let err = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "gradient descent optimization",
                "fusion_strategy": "bogus"
            }),
        )
        .await;
    assert!(err.is_err(), "invalid fusion_strategy must return an error");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("rrf")
            && msg.contains("weighted")
            && msg.contains("union")
            && msg.contains("vector_only")
            && msg.contains("keyword_only"),
        "error message must list valid strategies, got: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_score_floor() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "backpropagation neural network training algorithm",
                "salience": 0.6
            }),
        )
        .await
        .expect("remember succeeds");

    // Baseline: no floor — get result count
    let base = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "backpropagation neural network" }),
        )
        .await
        .expect("baseline recall succeeds");
    let base_count = base.as_array().expect("array").len();

    // score_floor=0.99 must not return MORE results than baseline
    let floored = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "backpropagation neural network",
                "score_floor": 0.99
            }),
        )
        .await
        .expect("recall with score_floor=0.99 succeeds");
    let floored_hits = floored.as_array().expect("array");
    assert!(
        floored_hits.len() <= base_count,
        "score_floor=0.99 must return ≤ baseline count ({base_count}), got {}",
        floored_hits.len()
    );

    // All returned hits must have score >= 0.99
    for hit in floored_hits {
        let score = hit["score"].as_f64().expect("score is a number");
        assert!(
            score >= 0.99,
            "score_floor=0.99: all returned scores must be ≥ 0.99, got {score}"
        );
    }

    // score_floor=0.0 must behave same as no floor
    let zero_floor = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "backpropagation neural network",
                "score_floor": 0.0
            }),
        )
        .await
        .expect("recall with score_floor=0.0 succeeds");
    let zero_count = zero_floor.as_array().expect("array").len();
    assert_eq!(
        zero_count, base_count,
        "score_floor=0.0 must return same count as no floor"
    );
}
