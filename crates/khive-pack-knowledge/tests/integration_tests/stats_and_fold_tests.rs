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
