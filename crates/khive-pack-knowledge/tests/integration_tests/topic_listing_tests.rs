// ── topic verb ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn topic_lists_all_concepts_without_filter() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "GQA", "domain": "attention" }),
    )
    .await
    .expect("learn 1");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "FlashAttention", "domain": "attention" }),
    )
    .await
    .expect("learn 2");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn 3");

    let resp = f
        .dispatch("knowledge.topic", json!({}))
        .await
        .expect("topic ok");

    let items = resp["results"].as_array().expect("results array");
    assert_eq!(items.len(), 3, "expected 3 concepts, got: {}", items.len());
}

#[tokio::test]
async fn topic_filters_by_domain() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "GQA", "domain": "attention" }),
    )
    .await
    .expect("learn 1");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "FlashAttention", "domain": "attention" }),
    )
    .await
    .expect("learn 2");
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn 3");

    let resp = f
        .dispatch("knowledge.topic", json!({ "domain": "attention" }))
        .await
        .expect("topic filtered");

    let items = resp["results"].as_array().expect("results array");
    assert_eq!(
        items.len(),
        2,
        "expected 2 attention concepts, got: {}",
        items.len()
    );

    let names: Vec<&str> = items.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"GQA"), "expected GQA in items: {names:?}");
    assert!(
        names.contains(&"FlashAttention"),
        "expected FlashAttention: {names:?}"
    );
}

#[tokio::test]
async fn topic_returns_empty_for_unknown_domain() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn");

    let resp = f
        .dispatch("knowledge.topic", json!({ "domain": "quantum-computing" }))
        .await
        .expect("topic ok");

    let items = resp["results"].as_array().expect("results array");
    assert!(items.is_empty(), "expected 0 items for unknown domain");
}

#[tokio::test]
async fn topic_respects_limit() {
    let f = pack(rt());
    for i in 0..5 {
        f.dispatch("knowledge.learn", json!({ "name": format!("Concept{i}") }))
            .await
            .expect("learn");
    }

    let resp = f
        .dispatch("knowledge.topic", json!({ "limit": 2 }))
        .await
        .expect("topic ok");

    let items = resp["results"].as_array().expect("results array");
    assert!(
        items.len() <= 2,
        "expected <= 2 items, got: {}",
        items.len()
    );
}

// ── H1 regression: case-insensitive domain filter (ADR-047 §91) ──────────────

#[tokio::test]
async fn topic_domain_filter_is_case_insensitive_listing_path() {
    let f = pack(rt());

    // Store concept with uppercase domain "Attention".
    f.dispatch(
        "knowledge.learn",
        json!({ "name": "FlashAttention", "domain": "Attention" }),
    )
    .await
    .expect("learn with Attention");

    f.dispatch(
        "knowledge.learn",
        json!({ "name": "LoRA", "domain": "fine-tuning" }),
    )
    .await
    .expect("learn with fine-tuning");

    // Query with lowercase "attention" — must find the concept stored as "Attention".
    let resp = f
        .dispatch("knowledge.topic", json!({ "domain": "attention" }))
        .await
        .expect("topic ok");

    let items = resp["results"].as_array().expect("results array");
    let names: Vec<&str> = items.iter().filter_map(|v| v["name"].as_str()).collect();
    assert_eq!(items.len(), 1, "expected 1 match, got: {names:?}");
    assert!(
        names.contains(&"FlashAttention"),
        "expected FlashAttention in results: {names:?}"
    );
    assert_eq!(
        resp["total"].as_u64().unwrap_or(0),
        1,
        "total should be 1 on listing path"
    );
}
