// ── knowledge.topic entity-map lookup regression ──────────────────────────────
//
// Regression for the latent `.unwrap()` in handle_topic (handlers.rs, search path).
//
// The old code did:
//
//   let filtered: Vec<_> = hits.into_iter().filter(|h| {
//       let Some(entity) = entity_map.get(&h.entity_id) else { return false; };
//       ...
//   }).collect();
//
//   let results = filtered.into_iter().map(|h| {
//       let entity = entity_map.get(&h.entity_id).unwrap(); // ← panic site
//       ...
//   });
//
// The fix merges the two passes into one filter_map that binds the entity once
// and carries the reference through to the map step.  No `.unwrap()` is needed.
//
// This test exercises the full query path of `knowledge.topic` with a query
// parameter (which triggers the search→entity_map→filter_map→results pipeline).
// It proves:
//   1. The verb succeeds without panicking.
//   2. Matching concepts are returned with the expected shape.
//   3. Domain post-filtering works correctly across both with-domain and
//      no-domain variants of the query path.

#[tokio::test]
async fn topic_query_path_does_not_panic_and_returns_results() {
    let f = pack(rt());

    // Seed two concept entities via knowledge.learn so they land in the KG
    // entity store (which is what handle_topic queries via hybrid_search +
    // get_entities_by_ids → entity_map).
    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "Retrieval Augmented Generation",
            "description": "technique combining dense retrieval with language model generation for knowledge-grounded responses unique topictest77a",
            "domain": "retrieval"
        }),
    )
    .await
    .expect("learn concept A");

    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "Contrastive Learning",
            "description": "self-supervised learning objective maximizing agreement between augmented views unique topictest77b",
            "domain": "training"
        }),
    )
    .await
    .expect("learn concept B");

    // Query path (query= supplied) — exercises hybrid_search → entity_map →
    // filter_map → results.  The filter_map must bind each entity once without
    // reaching any .unwrap() call.
    let resp = f
        .dispatch(
            "knowledge.topic",
            json!({ "query": "retrieval augmented generation unique topictest77a" }),
        )
        .await
        .expect("knowledge.topic must not panic");

    let results = resp["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "topic query must return at least one result: {resp:?}"
    );
    // Verify the expected entity appears with the required fields.
    let hit = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Retrieval Augmented Generation"))
        .expect("Retrieval Augmented Generation must appear in topic results");
    assert!(hit["id"].is_string(), "result must have id field: {hit:?}");
    assert!(
        hit["full_id"].is_string(),
        "result must have full_id field: {hit:?}"
    );
    assert!(
        hit["score"].is_number(),
        "result must have a numeric score: {hit:?}"
    );
}

#[tokio::test]
async fn topic_query_path_domain_filter_excludes_non_matching_concepts() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "HNSW Index",
            "description": "hierarchical navigable small world graph index for approximate nearest neighbor search unique topictest78a",
            "domain": "retrieval"
        }),
    )
    .await
    .expect("learn HNSW");

    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "Adam Optimizer",
            "description": "adaptive moment estimation optimizer for neural network training unique topictest78a",
            "domain": "training"
        }),
    )
    .await
    .expect("learn Adam");

    // With domain=retrieval: only HNSW should appear, Adam must be excluded.
    let resp = f
        .dispatch(
            "knowledge.topic",
            json!({
                "query": "approximate nearest neighbor optimizer unique topictest78a",
                "domain": "retrieval"
            }),
        )
        .await
        .expect("knowledge.topic with domain filter must not panic");

    let results = resp["results"].as_array().expect("results array");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Adam Optimizer"),
        "domain=retrieval must exclude training-domain concepts: {names:?}"
    );
}
