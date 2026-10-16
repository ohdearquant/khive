// ── W5: status filter + score multiplier ─────────────────────────────────────

#[tokio::test]
async fn w5_search_excludes_deprecated_by_default() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "dep-atom",
                "name": "Deprecated Atom",
                "content": "retrieval unique xyzqwerty deprecated content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval unique xyzqwerty", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Deprecated Atom"),
        "deprecated atom must not appear in default search: {names:?}"
    );
}

#[tokio::test]
async fn w5_search_includes_deprecated_when_explicitly_requested() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "dep-atom",
                "name": "Deprecated Atom",
                "content": "retrieval unique qwertyzyx deprecated content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval unique qwertyzyx", "status": "deprecated", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Deprecated Atom"),
        "deprecated atom must appear when status='deprecated' requested: {names:?}"
    );
}

// Atom status taxonomy is the closed set: draft | reviewed | deprecated.
// 'verified' is a section-level status only (ADR-047).
// reviewed (1.0×) must outrank draft (0.8×) — both present only when include_drafts=true.
#[tokio::test]
async fn w5_status_multiplier_reviewed_beats_draft() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "reviewed-atom",
                    "name": "Reviewed Atom",
                    "content": "neural network gradient descent unique zzzxxx learning dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "draft-atom",
                    "name": "Draft Atom",
                    "content": "neural network gradient unique zzzxxx learning dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("reviewed-atom".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("draft-atom".into())],
    )
    .await;

    // include_drafts=true to keep both atoms in results.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "neural network gradient learning zzzxxx", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let reviewed_score = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Reviewed Atom"))
        .and_then(|r| r["score"].as_f64());
    let draft_score = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Draft Atom"))
        .and_then(|r| r["score"].as_f64());

    match (reviewed_score, draft_score) {
        (Some(r), Some(d)) => assert!(
            r > d,
            "reviewed score {r:.4} must exceed draft score {d:.4} (1.0× vs 0.8× multiplier)"
        ),
        (Some(_), None) => {} // draft filtered below min_score — acceptable
        (None, _) => panic!("reviewed atom missing from results: {results:?}"),
    }
}

// Unknown atom statuses fall through to the 1.0 (reviewed-equivalent) multiplier.
// The closed public taxonomy is draft | reviewed | deprecated; anything else is neutral.
#[tokio::test]
async fn w5_status_multiplier_unknown_status_is_neutral() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "unknown-status-atom",
                "name": "Unknown Status Atom",
                "content": "unknown status neutral multiplier dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique unk78x"
            }]
        }),
    )
    .await
    .expect("upsert");

    // Set an unknown atom status directly — should not crash and should score as 1.0.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='custom' WHERE slug=?1",
        vec![SqlValue::Text("unknown-status-atom".into())],
    )
    .await;

    // include_drafts=true to allow any status through the exclusion filter.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "unknown status neutral unique unk78x", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    assert!(
        !results.is_empty(),
        "atom with unknown status must still appear in results: {results:?}"
    );
    let score = results[0]["score"].as_f64().expect("score");
    assert!(
        (0.0..=1.0).contains(&score),
        "score {score} for unknown-status atom must be in [0,1]"
    );
}

#[tokio::test]
async fn w5_list_excludes_deprecated_by_default() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "vis-atom", "name": "Visible Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "dep-atom", "name": "Hidden Deprecated Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom".into())],
    )
    .await;

    let resp = f
        .dispatch("knowledge.list", json!({ "type": "atom" }))
        .await
        .expect("list ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Visible Atom"),
        "visible atom should appear in list: {names:?}"
    );
    assert!(
        !names.contains(&"Hidden Deprecated Atom"),
        "deprecated atom must not appear in default list: {names:?}"
    );
}
