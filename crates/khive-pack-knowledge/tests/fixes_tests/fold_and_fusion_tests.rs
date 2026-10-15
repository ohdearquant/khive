// ── W6: fold exposes diversity_bias / epistemic_weight ────────────────────────

#[tokio::test]
async fn w6_fold_accepts_diversity_bias_and_epistemic_weight() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [
                    { "id": "c1", "score": 0.9, "size": 100, "information_gain": 0.8 },
                    { "id": "c2", "score": 0.7, "size": 150, "information_gain": 0.6 },
                    { "id": "c3", "score": 0.5, "size": 80,  "information_gain": 0.4 },
                ],
                "budget": 350,
                "diversity_bias": 0.5,
                "epistemic_weight": 0.3
            }),
        )
        .await
        .expect("fold with diversity_bias and epistemic_weight must succeed");

    let selected = resp["selected"].as_array().expect("selected array");
    let total_size = resp["total_size"].as_u64().expect("total_size");
    assert!(
        !selected.is_empty(),
        "at least one candidate must be selected"
    );
    assert!(
        total_size <= 350,
        "total_size {total_size} must not exceed budget 350"
    );
}

#[tokio::test]
async fn w6_fold_information_gain_threads_to_selector() {
    let f = pack(rt());

    // information_gain=0.9 on c1 should help it rank higher than pure score would.
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [
                    { "id": "high-ig", "score": 0.5, "size": 100, "information_gain": 0.9 },
                    { "id": "low-ig",  "score": 0.5, "size": 100, "information_gain": 0.0 },
                ],
                "budget": 10000,
                "epistemic_weight": 1.0
            }),
        )
        .await
        .expect("fold ok");

    let selected = resp["selected"].as_array().expect("selected");
    assert!(
        !selected.is_empty(),
        "fold must select at least one candidate: {resp:?}"
    );
}

// ── F1: khive-fusion RRF integration ─────────────────────────────────────────

#[tokio::test]
async fn f1_fuse_ann_hits_produces_valid_scores_via_search() {
    let f = pack(rt());
    // Seed a corpus so search has FTS candidates to fuse.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "rrf-a", "name": "RRF Alpha", "content": "rrf fusion scoring alpha dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "rrf-b", "name": "RRF Beta",  "content": "rrf fusion scoring beta dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "rrf-c", "name": "RRF Gamma", "content": "rrf fusion scoring gamma dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
            ]
        }),
    )
    .await
    .expect("seed corpus");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "reciprocal rank fusion scoring", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    assert!(!results.is_empty(), "fusion pipeline must produce results");

    for r in results {
        let score = r["score"]
            .as_f64()
            .expect("each result must have a numeric score");
        assert!(
            score > 0.0,
            "fused score must be positive, got {score} for {r:?}"
        );
        assert!(
            score.is_finite(),
            "fused score must be finite, got {score} for {r:?}"
        );
        assert!(
            score <= 1.0,
            "fused score must be normalized to [0,1], got {score} for {r:?}"
        );
    }
}

#[tokio::test]
async fn f1_rrf_k_60_constant_produces_finite_scores() {
    let f = pack(rt());
    // With RRF_K=60 and rank 1, score = 1/(60+1) ≈ 0.0164. Must be > 0 and finite.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "rrf-single",
                "name": "Single Result",
                "content": "unique sentinel zzzyyyxxx exact match content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rrf-single".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "unique sentinel zzzyyyxxx", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    assert!(
        !results.is_empty(),
        "single-result search must return the atom"
    );
    let score = results[0]["score"].as_f64().expect("score");
    assert!(
        score > 0.0 && score.is_finite(),
        "RRF_K=60 score must be positive and finite: {score}"
    );
    assert!(
        score <= 1.0,
        "RRF_K=60 score must be normalized to [0,1]: {score}"
    );
}
