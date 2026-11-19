// ── KG verbs round-tripped through the DSL ──────────────────────────────────

#[tokio::test]
async fn create_entity_via_dsl() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LoRA")"#,
    )
    .await?;
    assert_eq!(result["kind"], "concept");
    assert_eq!(result["name"], "LoRA");
    Ok(())
}

#[tokio::test]
async fn parallel_batch_of_independent_creates_all_succeed() -> anyhow::Result<()> {
    // Ops inside `[...]` are dispatched in parallel (ADR-016 §dispatch).
    // This test exercises that contract with independent ops only —
    // dependent ops (e.g. create-then-list) must split across two `request`
    // calls because the list won't see the creates inside the same batch.
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B"), create(kind="entity", entity_kind="concept", name="C")]"#
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("array");
    assert_eq!(results.len(), 3);
    for r in results {
        assert_eq!(r["ok"], json!(true), "op should succeed: {r}");
    }
    assert_eq!(body["summary"]["succeeded"], json!(3));
    assert_eq!(body["summary"]["failed"], json!(0));
    Ok(())
}

#[tokio::test]
async fn parallel_neighbors_results_echo_their_resolved_origin() -> anyhow::Result<()> {
    let client = connect().await?;
    let root_a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="neighbors-root-a")"#,
    )
    .await?;
    let root_b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="neighbors-root-b")"#,
    )
    .await?;
    let leaf = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="neighbors-leaf")"#,
    )
    .await?;
    let root_a_id = root_a["id"].as_str().expect("root A id");
    let root_b_id = root_b["id"].as_str().expect("root B id");
    let leaf_id = leaf["id"].as_str().expect("leaf id");

    ok_one(
        &client,
        &format!(r#"link(source_id="{root_a_id}", target_id="{root_b_id}", relation="extends")"#),
    )
    .await?;
    ok_one(
        &client,
        &format!(r#"link(source_id="{root_b_id}", target_id="{leaf_id}", relation="extends")"#),
    )
    .await?;

    let response = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"[neighbors(node_id="{root_a_id}", direction="out"), neighbors(node_id="{root_b_id}", direction="out")]"#
            ),
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&response))?;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);

    for (result, (expected_origin, expected_neighbor)) in results
        .iter()
        .zip([(root_a_id, root_b_id), (root_b_id, leaf_id)])
    {
        assert_eq!(result["ok"], json!(true), "neighbors op failed: {result}");
        let hits = result["result"].as_array().expect("neighbors result array");
        assert_eq!(hits.len(), 1, "unexpected neighbors result: {result}");
        assert_eq!(hits[0]["origin_id"], expected_origin);
        assert_eq!(hits[0]["id"], expected_neighbor);
    }
    Ok(())
}

#[tokio::test]
async fn create_then_list_across_separate_request_calls() -> anyhow::Result<()> {
    // Create-then-read requires two `request` calls because operations inside
    // a single batch run in parallel and have no ordering guarantee
    // (ADR-016 §dispatch).
    let client = connect().await?;
    call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B")]"#
        }),
    )
    .await?;

    let listed = ok_one(&client, r#"list(kind="entity")"#).await?;
    let entities = listed["items"]
        .as_array()
        .expect("entity list items envelope");
    let names: Vec<&str> = entities.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"A"), "entity A missing: {names:?}");
    assert!(names.contains(&"B"), "entity B missing: {names:?}");
    Ok(())
}

// ── ADR-103 Amendment 2: per-op `usage` object ──────────────────────────────

#[tokio::test]
async fn every_parallel_entry_carries_a_usage_object() -> anyhow::Result<()> {
    let client = connect().await?;
    let response = call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="concept", name="usage-a"), create(kind="badkind", name="usage-b")]"#
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&response))?;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    for entry in results {
        assert!(
            entry["usage"].is_object(),
            "every entry (ok and failed alike) carries a usage object: {entry}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn traverse_usage_counts_graph_work_per_op() -> anyhow::Result<()> {
    let client = connect().await?;
    let root = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="usage-traverse-root")"#,
    )
    .await?;
    let leaf = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="usage-traverse-leaf")"#,
    )
    .await?;
    let root_id = root["id"].as_str().expect("root id");
    let leaf_id = leaf["id"].as_str().expect("leaf id");
    ok_one(
        &client,
        &format!(r#"link(source_id="{root_id}", target_id="{leaf_id}", relation="extends")"#),
    )
    .await?;

    let response = call(
        &client,
        "request",
        json!({
            "ops": format!(r#"traverse(roots=["{root_id}"], max_depth=1)"#),
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&response))?;
    let entry = &body["results"][0];
    assert_eq!(entry["ok"], json!(true), "traverse failed: {entry}");
    let usage = entry["usage"].as_object().expect("usage object");
    assert!(
        usage
            .get("db_round_trips")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1,
        "traverse issues at least one batched storage round-trip: {usage:?}"
    );
    assert!(
        usage.get("graph_hops").and_then(Value::as_u64).unwrap_or(0) >= 1,
        "traverse returns at least one adjacency entry: {usage:?}"
    );
    Ok(())
}

#[tokio::test]
async fn propose_usage_counts_request_owned_event_rows() -> anyhow::Result<()> {
    let client = connect().await?;
    let response = call(
        &client,
        "request",
        json!({
            "ops": r#"propose(title="usage event-rows probe", description="request-owned event append must count", changeset={"kind": "add_entity", "entity": {"kind": "concept", "name": "UsageEventRowsEntity"}})"#,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&response))?;
    let entry = &body["results"][0];
    assert_eq!(entry["ok"], json!(true), "propose failed: {entry}");
    let usage = entry["usage"].as_object().expect("usage object");
    assert!(
        usage.get("event_rows").and_then(Value::as_u64).unwrap_or(0) >= 1,
        "propose appends a request-owned lifecycle event row that must count: {usage:?}"
    );
    Ok(())
}

#[tokio::test]
async fn chain_entries_each_carry_their_own_usage() -> anyhow::Result<()> {
    let client = connect().await?;
    let target = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="usage-chain-target")"#,
    )
    .await?;
    let target_id = target["id"].as_str().expect("target id");
    let response = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"create(kind="entity", entity_kind="concept", name="usage-chain-source") | link(source_id=$prev.id, target_id="{target_id}", relation="extends")"#
            ),
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&response))?;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    for entry in results {
        assert_eq!(entry["ok"], json!(true), "chain op failed: {entry}");
        assert!(
            entry["usage"].is_object(),
            "each chain entry carries its own usage object: {entry}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn invalid_kind_failure_does_not_abort_batch() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"[create(kind="entity", entity_kind="concept", name="ok"), create(kind="entity", entity_kind="bogus", name="bad")]"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["total"], 2);
    assert_eq!(body["summary"]["succeeded"], 1);
    assert_eq!(body["summary"]["failed"], 1);
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][1]["ok"], false);
    assert!(body["results"][1]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("bogus"));
    Ok(())
}

/// UE4-H2: empty batch `ops="[]"` must return an RPC-level `-32602 invalid_params`
/// error, not a `{results: [], summary: {total: 0}}` 200-style response.
#[tokio::test]
async fn empty_batch_returns_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let err = call(&client, "request", json!({"ops": "[]"})).await.err();
    let svc = err.as_ref().and_then(|e| e.downcast_ref::<ServiceError>());
    assert!(
        matches!(
            svc,
            Some(ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS
        ),
        "UE4-H2: empty batch must return INVALID_PARAMS, got {err:?}"
    );
    // Also check JSON-form empty array.
    let err2 = call(&client, "request", json!({"ops": "[]"})).await.err();
    let svc2 = err2.as_ref().and_then(|e| e.downcast_ref::<ServiceError>());
    assert!(
        matches!(
            svc2,
            Some(ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS
        ),
        "UE4-H2: empty JSON batch must return INVALID_PARAMS, got {err2:?}"
    );
    Ok(())
}

#[tokio::test]
async fn malformed_dsl_returns_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let err = call(&client, "request", json!({"ops": "create("}))
        .await
        .err();
    let svc = err.as_ref().and_then(|e| e.downcast_ref::<ServiceError>());
    assert!(
        matches!(
            svc,
            Some(ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS
        ),
        "expected invalid_params for malformed DSL, got {err:?}"
    );
    Ok(())
}
