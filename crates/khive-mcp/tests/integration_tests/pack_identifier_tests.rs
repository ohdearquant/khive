// ── GTD verbs round-tripped through the DSL ─────────────────────────────────

#[tokio::test]
async fn assign_then_next_then_complete() -> anyhow::Result<()> {
    let client = connect().await?;

    let assigned = ok_one(
        &client,
        r#"gtd.assign(title="ship release", status="next", priority="p0")"#,
    )
    .await?;
    let id = assigned["full_id"].as_str().unwrap().to_string();
    assert_eq!(assigned["kind"], "task");
    assert_eq!(assigned["status"], "next");

    let next_list = ok_one(&client, "gtd.next()").await?;
    let arr = next_list.as_array().unwrap();
    assert!(arr.iter().any(|t| t["full_id"] == id));

    let completed = ok_one(
        &client,
        &format!(r#"gtd.complete(id="{id}", result="shipped via request")"#),
    )
    .await?;
    assert_eq!(completed["to"], "done");
    Ok(())
}

#[tokio::test]
async fn gtd_context_entity_id_round_trips_in_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;
    let context = ok_one(&client, r#"create(kind="concept", name="Agent Context")"#).await?;
    let context_id = context["id"]
        .as_str()
        .expect("create must expose a full chaining id");

    let assigned = agent_one(
        &client,
        &format!(r#"gtd.assign(title="strict context one", context_entity_id="{context_id}")"#),
    )
    .await?;
    let returned = assigned["context_entity_id"]
        .as_str()
        .expect("Agent response must retain context_entity_id");
    assert_eq!(returned, context_id);
    assert_eq!(assigned["properties"]["context_entity_id"], context_id);
    assert_eq!(
        returned.len(),
        36,
        "strict response id must remain canonical"
    );

    agent_one(
        &client,
        &format!(r#"gtd.assign(title="strict context two", context_entity_id="{returned}")"#),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn memory_feedback_target_id_round_trips_in_agent_mode() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let remembered = ok_one(
        &client,
        r#"memory.remember(content="strict feedback target", salience=0.7)"#,
    )
    .await?;
    let target_id = remembered["id"]
        .as_str()
        .expect("remember must expose a full chaining id");

    let feedback = agent_one(
        &client,
        &format!(r#"memory.feedback(target_id="{target_id}", signal="useful")"#),
    )
    .await?;
    let returned = feedback["target_id"]
        .as_str()
        .expect("feedback must acknowledge its exact target");
    assert_eq!(returned, target_id);
    assert_eq!(returned.len(), 36, "feedback target must stay canonical");

    agent_one(
        &client,
        &format!(r#"memory.feedback(target_id="{returned}", signal="useful")"#),
    )
    .await?;
    Ok(())
}

/// #1762: a recalled memory must expose a canonical handle in the default
/// Agent presentation so the next request can feed it directly to the strict
/// `memory.feedback(target_id=...)` contract without an intervening `get`.
#[tokio::test]
async fn memory_recall_full_id_chains_to_feedback_in_agent_mode() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let remembered = ok_one(
        &client,
        r#"memory.remember(content="identifier continuity recall sentinel", salience=0.9)"#,
    )
    .await?;
    let expected_id = remembered["id"]
        .as_str()
        .expect("remember must return a canonical id");

    let recalled = agent_one(
        &client,
        r#"memory.recall(query="identifier continuity recall sentinel", limit=10, fusion_strategy="keyword_only", min_score=0.0)"#,
    )
    .await?;
    let hit = recalled
        .as_array()
        .expect("recall result must be an array")
        .iter()
        .find(|row| row["full_id"] == expected_id)
        .expect("recall must expose the remembered row by canonical full_id");
    let full_id = hit["full_id"]
        .as_str()
        .expect("recall hit must expose full_id");
    assert_eq!(full_id.len(), 36, "full_id must remain canonical");

    let feedback = agent_one(
        &client,
        &format!(r#"memory.feedback(target_id="{full_id}", signal="useful")"#),
    )
    .await?;
    assert_eq!(feedback["target_id"], full_id);
    Ok(())
}

/// #1762: session summaries keep a canonical cross-request handle even though
/// Agent presentation may compact their display-oriented `id` field.
#[tokio::test]
async fn session_list_full_id_chains_to_resume_in_agent_mode() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let stored = ok_one(
        &client,
        r#"session.store(content="identifier continuity session sentinel", title="continuity sentinel")"#,
    )
    .await?;
    let expected_id = stored["session"]["id"]
        .as_str()
        .expect("session.store must return a canonical session id");

    let listed = agent_one(&client, r#"session.list(limit=20)"#).await?;
    let summary = listed["sessions"]
        .as_array()
        .expect("session.list must return summaries")
        .iter()
        .find(|row| row["full_id"] == expected_id)
        .expect("session.list must expose the stored session by canonical full_id");
    let full_id = summary["full_id"]
        .as_str()
        .expect("session summary must expose full_id");
    assert_eq!(full_id.len(), 36, "full_id must remain canonical");

    let resumed = agent_one(&client, &format!(r#"session.resume(id="{full_id}")"#)).await?;
    assert_eq!(
        resumed["session"]["id"].as_str(),
        Some(&full_id[..8]),
        "the canonical list handle must be accepted even though the Standard resume response compacts id"
    );
    Ok(())
}

/// `brain.auto_feedback` acknowledges the canonical target it credited; that
/// acknowledgement feeds the strict `memory.feedback`/`brain.feedback`
/// target_id parameter, so the default Agent transform must not shorten it
/// (the verb is AlwaysVerbose for the same reason `brain.feedback` is).
#[tokio::test]
async fn brain_auto_feedback_target_id_round_trips_in_agent_mode() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let remembered = ok_one(
        &client,
        r#"memory.remember(content="auto feedback strict target", salience=0.7)"#,
    )
    .await?;
    let target_id = remembered["id"]
        .as_str()
        .expect("remember must expose a full chaining id");

    let ops = serde_json::to_string(&json!([{
        "tool": "brain.auto_feedback",
        "args": {
            "query": "auto feedback strict target round trip",
            "results": [{"id": target_id}],
            "signal": "useful",
            "target_id": target_id,
        }
    }]))?;
    // No `presentation` key — defaults to Agent; AlwaysVerbose must override.
    let result = call(&client, "request", json!({ "ops": ops })).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "auto_feedback must succeed: {first}");
    let returned = first["result"]["target_id"]
        .as_str()
        .expect("auto_feedback must acknowledge its credited target");
    assert_eq!(
        returned.len(),
        36,
        "auto_feedback target_id must stay canonical in Agent mode; got {returned:?}"
    );
    assert_eq!(returned, target_id);

    agent_one(
        &client,
        &format!(r#"memory.feedback(target_id="{returned}", signal="useful")"#),
    )
    .await?;
    Ok(())
}
