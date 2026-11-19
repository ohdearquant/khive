#[tokio::test]
async fn proposal_identifiers_round_trip_in_agent_mode() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let root_ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "strict proposal root",
            "description": "root for identifier round-trip coverage",
            "changeset": {
                "kind": "add_entity",
                "entity": {"kind": "concept", "name": "strict proposal root entity"}
            }
        }
    }]))?;
    let root = agent_one(&client, &root_ops).await?;
    assert_eq!(
        root["id"].as_str().map(str::len),
        Some(8),
        "ordinary Agent identifiers may stay compact"
    );
    let root_full_id = root["full_id"]
        .as_str()
        .expect("proposal must expose its canonical chaining id");
    assert_eq!(root_full_id.len(), 36);

    let child_ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "strict proposal child",
            "description": "child carrying the exact ancestry handle",
            "changeset": {
                "kind": "add_entity",
                "entity": {"kind": "concept", "name": "strict proposal child entity"}
            },
            "parent_id": root_full_id
        }
    }]))?;
    let child = agent_one(&client, &child_ops).await?;
    let returned_parent = child["parent_id"]
        .as_str()
        .expect("Agent response must retain exact proposal ancestry");
    assert_eq!(returned_parent, root_full_id);
    assert_eq!(returned_parent.len(), 36);

    let grandchild_ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "strict proposal grandchild",
            "description": "reuses the returned parent handle unchanged",
            "changeset": {
                "kind": "add_entity",
                "entity": {"kind": "concept", "name": "strict proposal grandchild entity"}
            },
            "parent_id": returned_parent
        }
    }]))?;
    agent_one(&client, &grandchild_ops).await?;

    for invalid_args in [
        json!({
            "title": "short proposal parent",
            "description": "must reject a parent prefix",
            "changeset": {
                "kind": "add_entity",
                "entity": {"kind": "concept", "name": "invalid parent prefix entity"}
            },
            "parent_id": root["id"],
        }),
        json!({
            "title": "short nested changeset id",
            "description": "must reject a nested identifier prefix",
            "changeset": {
                "kind": "update_entity",
                "id": "deadbeef",
                "patch": {"name": "not applied"}
            },
        }),
    ] {
        let invalid_ops = serde_json::to_string(&json!([{
            "tool": "propose",
            "args": invalid_args,
        }]))?;
        let response = call(&client, "request", json!({"ops": invalid_ops})).await?;
        let body: Value = serde_json::from_str(&first_text(&response))?;
        let failure = &body["results"][0];
        assert_eq!(failure["ok"], false, "{failure}");
        let error = &failure["error"];
        let message = error["message"]
            .as_str()
            .or_else(|| error.as_str())
            .or_else(|| failure["message"].as_str())
            .unwrap_or("");
        assert!(
            message.contains("short prefix") || message.contains("short-prefix"),
            "rejection must explain prefix-resolution consequence: {message}"
        );
        assert!(
            message.contains("stable"),
            "rejection must explain why exact identity matters: {message}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn event_session_id_round_trips_in_agent_mode() -> anyhow::Result<()> {
    let (client, session_id) = connect_with_session_event().await?;
    let first = agent_one(
        &client,
        &format!(
            r#"list(kind="event", namespace="eventroundtrip", session_id="{session_id}", limit=10)"#
        ),
    )
    .await?;
    let events = first["items"]
        .as_array()
        .expect("event list items envelope");
    assert_eq!(events.len(), 1, "session filter must select the fixture");
    let returned = events[0]["session_id"]
        .as_str()
        .expect("Agent event must retain its exact session filter handle");
    assert_eq!(returned, session_id.as_hyphenated().to_string());
    assert_eq!(returned.len(), 36);

    let second = agent_one(
        &client,
        &format!(
            r#"list(kind="event", namespace="eventroundtrip", session_id="{returned}", limit=10)"#
        ),
    )
    .await?;
    assert_eq!(
        second["items"].as_array().map(Vec::len),
        Some(1),
        "the returned session_id must be accepted unchanged"
    );
    Ok(())
}

#[tokio::test]
async fn transition_lifecycle_rejection_is_per_op_not_protocol_error() -> anyhow::Result<()> {
    let client = connect().await?;
    let assigned = ok_one(&client, r#"gtd.assign(title="lifecycle")"#).await?;
    let id = assigned["full_id"].as_str().unwrap().to_string();

    // inbox → done is allowed; done → inbox is NOT.
    ok_one(
        &client,
        &format!(r#"gtd.transition(id="{id}", status="done")"#),
    )
    .await?;

    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"gtd.transition(id="{id}", status="inbox")"#)}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    // Per P15, terminal states (done/cancelled) reject ALL outgoing
    // transitions with "task X is in terminal state Y; no further transitions allowed".
    assert!(
        first["error"]["message"]
            .as_str()
            .unwrap()
            .contains("terminal state"),
        "expected terminal-state rejection, got: {}",
        first["error"]
    );
    Ok(())
}

#[tokio::test]
async fn parallel_assign_batch_creates_n_tasks() -> anyhow::Result<()> {
    let client = connect().await?;
    let ops = r#"[
        gtd.assign(title="t1", priority="p0"),
        gtd.assign(title="t2", priority="p1"),
        gtd.assign(title="t3", priority="p2")
    ]"#;
    let result = call(&client, "request", json!({"ops": ops})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["succeeded"], 3, "{body}");
    Ok(())
}

#[tokio::test]
async fn unknown_verb_returns_per_op_failure_not_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(&client, "request", json!({"ops": "retire()"})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    assert!(first["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown verb"));
    Ok(())
}
