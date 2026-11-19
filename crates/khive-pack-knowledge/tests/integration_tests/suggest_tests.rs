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
