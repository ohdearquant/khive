// ── Cross-pack deny_unknown_fields ─────────────────────────────────────────

/// `create(kind="concept", unknownkw="x")` must return `ok: false`.
#[tokio::test]
async fn create_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"create(kind="concept", name="X", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "create with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `assign(title="T", unknownkw="x")` (GTD) must return `ok: false`.
#[tokio::test]
async fn assign_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"gtd.assign(title="GTD unknown kwarg test", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "assign with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `send(to="x", content="y", unknownkw="z")` (comm) must return `ok: false`.
#[tokio::test]
async fn send_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_comm_only().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"comm.send(to="alice", content="test", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "send with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `agenda(unknownkw="x")` (schedule) must return `ok: false`.
#[tokio::test]
async fn agenda_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_schedule().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"schedule.agenda(unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "agenda with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `brain.profile(id="balanced-recall-v1", unknownkw="x")` must return `ok: false`.
#[tokio::test]
async fn brain_profile_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_brain_only().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"brain.profile(id="balanced-recall-v1", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "brain.profile with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

fn make_knowledge_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("knowtest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+knowledge runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+knowledge")
}

async fn connect_knowledge(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_knowledge_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `topic(unknownkw="x")` (knowledge) must return `ok: false`.
#[tokio::test]
async fn topic_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_knowledge().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"knowledge.topic(unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "topic with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"]["message"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

// ── brain.feedback default Agent response preserves full target_id ────────────

/// `brain.feedback` in default Agent mode must return `target_id` as the full
/// 36-char UUID, not the 8-char Agent-mode prefix.
#[tokio::test]
async fn brain_feedback_default_agent_response_preserves_full_target_id() -> anyhow::Result<()> {
    let client = connect_brain_only().await?;

    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="BrainFeedbackTarget")"#,
    )
    .await?;
    let target_id = created["id"]
        .as_str()
        .expect("created entity id")
        .to_string();
    assert_eq!(
        target_id.len(),
        36,
        "entity id from verbose ok_one must be 36-char"
    );

    // Use plain `call` (not `ok_one`) so Agent mode is not forced to verbose.
    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"brain.feedback(target_id="{target_id}", signal="useful")"#)}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "brain.feedback must succeed: {first}"
    );

    let returned = first["result"]["target_id"].as_str().unwrap_or("");
    assert_eq!(
        returned.len(),
        36,
        "brain.feedback Agent response target_id must be full 36-char UUID, got: {returned:?}"
    );
    assert_eq!(returned, target_id, "returned target_id must match input");
    Ok(())
}
