// ── Granular `kind=<specific>` discriminator (no entity_kind / note_kind) ────

#[tokio::test]
async fn create_with_granular_entity_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="concept", name="GraphAttention", description="self-attention over graph neighborhoods")"#,
    )
    .await?;
    assert_eq!(result["kind"], "concept", "stored under concept kind");
    assert_eq!(result["name"], "GraphAttention");
    Ok(())
}

#[tokio::test]
async fn create_with_granular_note_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="observation", content="qwen3.5 retains long-context recall up to 64k")"#,
    )
    .await?;
    assert_eq!(
        result["kind"], "observation",
        "stored under observation kind"
    );
    Ok(())
}

#[tokio::test]
async fn create_granular_kind_conflicts_with_legacy_subfield() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"create(kind="concept", entity_kind="document", name="Conflict")"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false, "expected contradiction error: {first}");
    let err = first["error"]["message"].as_str().unwrap();
    assert!(
        err.contains("contradicts"),
        "error should explain the contradiction: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn list_with_granular_entity_kind_filters_results() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"create(kind="concept", name="GranularListA")"#).await?;
    ok_one(&client, r#"create(kind="document", name="GranularListB")"#).await?;

    let listed = ok_one(&client, r#"list(kind="concept")"#).await?;
    let arr = listed["items"].as_array().expect("list items envelope");
    let names: Vec<&str> = arr.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(
        names.contains(&"GranularListA"),
        "concept missing: {names:?}"
    );
    assert!(
        !names.contains(&"GranularListB"),
        "document leaked into concept filter: {names:?}"
    );
    Ok(())
}

#[tokio::test]
async fn list_with_granular_task_kind_lists_only_tasks() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"gtd.assign(title="GranularTaskA")"#).await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="not a task")"#,
    )
    .await?;

    let listed = ok_one(&client, r#"list(kind="task")"#).await?;
    let arr = listed["items"].as_array().expect("list items envelope");
    let titles: Vec<&str> = arr.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(
        titles.contains(&"GranularTaskA"),
        "task missing: {titles:?}"
    );
    assert!(
        !titles.iter().any(|t| t.contains("not a task")),
        "observation leaked into task list: {titles:?}"
    );
    Ok(())
}

#[tokio::test]
async fn search_with_granular_entity_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(
        &client,
        r#"create(kind="concept", name="HybridSearchConcept", description="needle for search")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="document", name="HybridSearchDocument", description="needle for search")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="concept", query="HybridSearch needle", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected at least one hit");
    // Verify the hit kind: fetch each via get and assert kind=concept.
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["kind"], "concept",
            "search(kind=\"concept\") returned non-concept: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_with_granular_task_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"gtd.assign(title="urgent search needle one")"#).await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="urgent search needle two")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="task", query="urgent search needle", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected task hits");
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["kind"], "task",
            "search(kind=\"task\") returned non-task: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_substrate_wide_note_kind_still_works() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(
        &client,
        r#"gtd.assign(title="quasiparticle task entry", description="quasiparticle decoherence backlog")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="quasiparticle decoherence drives loss in transmons")"#,
    )
    .await?;

    // Backwards-compat: kind="note" still ranges over every note kind.
    let hits = ok_one(
        &client,
        r#"search(kind="note", query="quasiparticle decoherence", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(
        arr.len() >= 2,
        "kind=note should range over task AND observation; got {arr:?}"
    );
    Ok(())
}

#[tokio::test]
async fn search_unknown_kind_lists_all_valid_options() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"search(kind="bogus", query="anything")"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    let err = first["error"]["message"].as_str().unwrap();
    assert!(err.contains("bogus"), "error names the bad kind: {err}");
    // The merged list must include substrate-level + pack-registered kinds.
    for expected in ["entity", "note", "edge", "concept", "task"] {
        assert!(
            err.contains(expected),
            "error must list {expected:?}: {err}"
        );
    }
    Ok(())
}

// ── Sub-filter contract: substrate `kind` + legacy `entity_kind`/`note_kind` ──

#[tokio::test]
async fn search_substrate_kind_entity_with_legacy_entity_kind_sub_filter() -> anyhow::Result<()> {
    // ADR-023 §`kind` parameter: substrate `kind="entity"` must honor the
    // legacy `entity_kind` sub-filter and behave identically to granular form.
    let client = connect().await?;
    ok_one(
        &client,
        r#"create(kind="concept", name="SubFilterEntityConcept", description="zaphod beeblebrox marker")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="document", name="SubFilterEntityDoc", description="zaphod beeblebrox marker")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="entity", entity_kind="concept", query="zaphod beeblebrox", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected concept hits, got: {arr:?}");
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["kind"], "concept",
            "search(kind=\"entity\", entity_kind=\"concept\") returned non-concept: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_substrate_kind_note_with_legacy_note_kind_sub_filter() -> anyhow::Result<()> {
    // ADR-023 §`kind` parameter: substrate `kind="note"` must honor the
    // legacy `note_kind` sub-filter and behave identically to granular form.
    let client = connect().await?;
    ok_one(
        &client,
        r#"gtd.assign(title="ghyll task entry", description="ghyll mistral foxtrot marker")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="ghyll mistral foxtrot marker observation")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="note", note_kind="task", query="ghyll mistral foxtrot", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected task hits, got: {arr:?}");
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["kind"], "task",
            "search(kind=\"note\", note_kind=\"task\") returned non-task: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_granular_kind_contradicting_legacy_subfield_is_rejected() -> anyhow::Result<()> {
    // ADR-023 §`kind` parameter contradiction rule: granular `kind="concept"`
    // with `entity_kind="document"` must be rejected, not silently coerced.
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"search(kind="concept", entity_kind="document", query="anything", limit=5)"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false, "expected contradiction error: {first}");
    let err = first["error"]["message"].as_str().unwrap();
    assert!(
        err.contains("contradicts"),
        "error should explain the contradiction: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn search_kind_filter_surfaces_right_kind_when_wrong_kind_outranks() -> anyhow::Result<()> {
    // Regression: previously the kind filter applied AFTER truncating fused
    // candidates to `limit`, so right-kind hits ranked below `limit` got
    // dropped. The fix defers truncation until after the alive+kind filter.
    //
    // Setup: 5 documents matching the query (likely to dominate the top of
    // the fused list) + 1 concept matching the same query. With limit=2,
    // pre-fix would return 0 hits when the top-2 fused are all documents;
    // post-fix the kind filter retains the lone concept from the wider
    // candidate pool (limit * 4 = 8).
    let client = connect().await?;
    for i in 0..5 {
        ok_one(
            &client,
            &format!(
                r#"create(kind="document", name="WrongKindDoc{i}", description="orthogonal wavelet quibble marker")"#
            ),
        )
        .await?;
    }
    ok_one(
        &client,
        r#"create(kind="concept", name="RightKindConcept", description="orthogonal wavelet quibble marker")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="concept", query="orthogonal wavelet quibble", limit=2)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(
        !arr.is_empty(),
        "right-kind hit must surface even when wrong-kind hits outrank it; got: {arr:?}"
    );
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["kind"], "concept",
            "search(kind=\"concept\") must only return concepts: {got}"
        );
    }
    Ok(())
}
