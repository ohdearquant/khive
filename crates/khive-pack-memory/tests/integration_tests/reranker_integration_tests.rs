// ── Reranker integration tests ──────────────────────────────────────────────

/// Empty reranker_weights is a pass-through — results must be identical
/// to a baseline recall with no reranker config.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_with_empty_reranker_weights_is_passthrough() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    for i in 0..4 {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("memory about deep learning topic {i}"),
                    "salience": 0.5 + (i as f64) * 0.1,
                    "decay": 0.0
                }),
            )
            .await
            .expect("memory.remember");
    }

    let baseline = registry
        .dispatch("memory.recall", json!({ "query": "deep learning" }))
        .await
        .expect("baseline recall");
    let baseline_ids: Vec<String> = baseline
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();

    let with_empty_reranker = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "deep learning",
                "config": { "reranker_weights": {} }
            }),
        )
        .await
        .expect("recall with empty reranker_weights");
    let reranker_ids: Vec<String> = with_empty_reranker
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        baseline_ids, reranker_ids,
        "empty reranker_weights must be a pass-through — result ordering must match baseline"
    );
}

/// reranker_weights with salience=1.0 must promote the highest-salience
/// memory to rank #1, even when it would rank lower under the default compute_score.
///
/// Strengthened: captures baseline ordering first (no reranker) and asserts that
/// the reranked order actually differs — proving the REPLACE wiring is not a no-op.
///
/// Fixture design: all notes contain the query keyword so all are retrieved.
/// Low-salience notes have richer keyword density (higher FTS BM25).  Baseline
/// uses pure relevance scoring (salience_weight=0) so the keyword-dense
/// low-salience notes rank first.  The salience=1.0 reranker then flips the
/// order, placing the high-salience note at rank #1.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_with_reranker_weights_changes_ordering() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Three low-salience notes with high keyword density for "gradient descent" —
    // their BM25 score will be higher than the high-salience note.
    for _ in 0..3 {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": "gradient descent gradient descent gradient descent optimization",
                    "salience": 0.1,
                    "decay": 0.0
                }),
            )
            .await
            .expect("low salience remember");
    }

    // One high-salience note that mentions gradient descent only once — lower BM25
    // relevance so baseline (pure-relevance) ranks it below the low-salience notes.
    let high_salience = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "gradient descent is a key technique in machine learning",
                "salience": 0.95,
                "decay": 0.0
            }),
        )
        .await
        .expect("high salience remember");
    let high_id = high_salience["id"].as_str().unwrap().to_string();

    // Step 1: baseline recall — pure relevance scoring (salience_weight=0) so
    // BM25-heavy low-salience notes rank first.
    let baseline = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "gradient descent",
                "config": {
                    "relevance_weight": 1.0,
                    "salience_weight": 0.0,
                    "temporal_weight": 0.0
                }
            }),
        )
        .await
        .expect("baseline recall");
    let baseline_hits = baseline.as_array().expect("baseline array");
    assert!(
        baseline_hits.len() >= 2,
        "need at least 2 results to test ordering change, got {}",
        baseline_hits.len()
    );
    let baseline_ids: Vec<String> = baseline_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    let baseline_top = &baseline_ids[0];

    // Baseline must NOT have high_id at rank #1 — if it does, the fixture is
    // degenerate (the reranker would be a no-op for the top position).
    assert_ne!(
        baseline_top, &high_id,
        "fixture error: high-salience note already ranks first in baseline; \
         reranker change cannot be demonstrated. baseline={baseline_ids:?}"
    );

    // Step 2: reranked recall — salience weight only (REPLACE strategy).
    let reranked = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "gradient descent",
                "config": {
                    "reranker_weights": { "salience": 1.0 }
                }
            }),
        )
        .await
        .expect("recall with salience reranker");
    let reranked_hits = reranked.as_array().expect("reranked array");
    assert!(!reranked_hits.is_empty(), "must get results");
    let reranked_ids: Vec<String> = reranked_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    let top_id = &reranked_ids[0];

    // Step 3: assert the reranker placed high-salience memory at rank #1.
    assert_eq!(
        top_id, &high_id,
        "salience=1.0 reranker must rank the highest-salience memory first; got {top_id} not {high_id}"
    );

    // Step 4: assert the ordering actually changed — the reranker is not a no-op.
    // baseline_top != high_id (asserted above) and top_id == high_id, so orderings differ.
    assert_ne!(
        baseline_ids, reranked_ids,
        "reranker must change the result ordering; baseline={baseline_ids:?} reranked={reranked_ids:?}"
    );
}

/// The recall.rerank subhandler applies request weights and returns
/// non-zero rerank_scores when reranker_weights are provided.
#[tokio::test]
async fn test_rerank_subhandler_uses_request_weights() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    // Build two synthetic fused candidates with different fused_scores.
    // The one with higher fused_score should get a higher rerank_score
    // when relevance weight = 1.0.
    let candidates = json!([
        {
            "id": "00000000-0000-0000-0000-000000000001",
            "fused_score": 0.9,
            "source": "both"
        },
        {
            "id": "00000000-0000-0000-0000-000000000002",
            "fused_score": 0.3,
            "source": "text"
        }
    ]);

    let result = registry
        .dispatch(
            "memory.recall_rerank",
            json!({
                "candidates": candidates,
                "config": {
                    "reranker_weights": { "relevance": 1.0 }
                }
            }),
        )
        .await
        .expect("recall.rerank succeeds");

    let reranked = result["reranked"].as_array().expect("reranked array");
    assert_eq!(reranked.len(), 2, "both candidates returned");

    // Find scores by note_id.
    let score_for = |id: &str| -> f64 {
        reranked
            .iter()
            .find(|c| c["id"].as_str() == Some(id))
            .and_then(|c| c["rerank_score"].as_f64())
            .unwrap_or(f64::NAN)
    };
    let score_high = score_for("00000000-0000-0000-0000-000000000001");
    let score_low = score_for("00000000-0000-0000-0000-000000000002");

    assert!(
        score_high.is_finite() && score_low.is_finite(),
        "rerank_score must be a finite number; got high={score_high} low={score_low}"
    );
    assert!(
        score_high > score_low,
        "candidate with fused_score=0.9 must outscore fused_score=0.3 under relevance reranker; \
         got {score_high} vs {score_low}"
    );

    // Verify active_rerankers field is present.
    let active = result["active_rerankers"]
        .as_array()
        .expect("active_rerankers");
    assert!(
        active.iter().any(|v| v.as_str() == Some("relevance")),
        "active_rerankers must include 'relevance'"
    );
}
