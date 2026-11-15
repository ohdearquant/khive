// ── help=true schema envelope integration tests ─────────────────────────────
//
// These tests confirm that help=true calls through the MCP surface return
// non-empty params slices with specific known parameters — verifying that
// the HandlerDef.params slices are populated (not left as &[]).

async fn connect_full(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_full_server();
    tokio::spawn(async move {
        if let Ok(server_service) = server.serve(server_transport).await {
            let _ = server_service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

async fn connect_with_session_event() -> anyhow::Result<(
    impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    uuid::Uuid,
)> {
    disable_daemon();
    let namespace = Namespace::parse("eventroundtrip")?;
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: namespace.clone(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    })?;
    let token = runtime.authorize(namespace)?;
    let session_id = uuid::Uuid::new_v4();
    runtime
        .events(&token)?
        .append_event(
            Event::new(
                token.namespace().as_str(),
                "session_roundtrip_fixture",
                EventKind::Audit,
                SubstrateKind::Entity,
                "test",
            )
            .with_session_id(session_id),
        )
        .await?;

    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = KhiveMcpServer::new(runtime)?;
    tokio::spawn(async move {
        if let Ok(server_service) = server.serve(server_transport).await {
            let _ = server_service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok((client, session_id))
}

/// Helper: call `verb(help=true)` through the MCP surface and return the
/// parsed result. Asserts the op succeeded and returns the schema envelope.
async fn help_schema(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    verb: &str,
) -> anyhow::Result<Value> {
    let ops = format!("{verb}(help=true)");
    let result = call(client, "request", json!({"ops": &ops})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = body["results"].get(0).cloned().unwrap_or(Value::Null);
    assert_eq!(
        first["ok"],
        json!(true),
        "{verb}(help=true) must succeed, got: {first}"
    );
    Ok(first["result"].clone())
}

#[tokio::test]
async fn help_includes_shared_identifier_resolution_contract() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "gtd.assign").await?;
    let contract = &schema["identifier_resolution"];

    assert!(contract["full_uuid"]
        .as_str()
        .is_some_and(|text| text.contains("globally unique")));
    assert!(contract["short_prefix"]
        .as_str()
        .is_some_and(|text| text.contains("lookup scope belongs to the consuming parameter")));
    assert!(contract["parameter_rule"]
        .as_str()
        .is_some_and(|text| text.contains("corresponding response field")));
    Ok(())
}

#[tokio::test]
async fn help_recall_params_non_empty_with_query_param() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "memory.recall").await?;
    let params = schema["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "recall help=true must return non-empty params; got empty slice"
    );
    let has_query = params.iter().any(|p| p["name"] == json!("query"));
    assert!(
        has_query,
        "recall params must include 'query'; got: {params:?}"
    );
    Ok(())
}

#[tokio::test]
async fn help_brain_feedback_params_non_empty_with_target_and_signal() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "brain.feedback").await?;
    let params = schema["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "brain.feedback help=true must return non-empty params"
    );
    let has_id = params.iter().any(|p| p["name"] == json!("id"));
    assert!(
        has_id,
        "brain.feedback params must include 'id'; got: {params:?}"
    );
    assert!(
        !params.iter().any(|p| p["name"] == json!("target_id")),
        "brain.feedback must not advertise legacy 'target_id'; got: {params:?}"
    );
    let has_signal = params.iter().any(|p| p["name"] == json!("signal"));
    assert!(
        has_signal,
        "brain.feedback params must include 'signal'; got: {params:?}"
    );
    Ok(())
}

#[tokio::test]
async fn help_brain_actor_reads_expose_caller_scope_and_fleet_gate() -> anyhow::Result<()> {
    let client = connect_full().await?;
    for verb in ["brain.event_counts", "brain.resolve", "brain.bindings"] {
        let schema = help_schema(&client, verb).await?;
        let description = schema["description"]
            .as_str()
            .expect("verb help description")
            .to_ascii_lowercase();
        assert!(description.contains("caller"), "{verb}: {description}");
        let params = schema["params"].as_array().expect("help params");
        let actor = params
            .iter()
            .find(|param| param["name"] == "actor")
            .expect("actor read help includes actor");
        assert_eq!(actor["type"], "string", "{verb}: {actor}");
        assert_eq!(actor["required"], false, "{verb}: {actor}");
        let actor_scope = actor["description"]
            .as_str()
            .expect("actor scope description")
            .to_ascii_lowercase();
        assert!(actor_scope.contains("caller"), "{verb}: {actor_scope}");
        assert!(actor_scope.contains("visible"), "{verb}: {actor_scope}");

        let all_actors = params.iter().find(|param| param["name"] == "all_actors");
        if verb == "brain.event_counts" {
            let all_actors = all_actors.expect("event counts advertises fleet reads");
            assert_eq!(all_actors["type"], "boolean");
            assert_eq!(all_actors["required"], false);
            let fleet_scope = all_actors["description"]
                .as_str()
                .expect("fleet read scope description")
                .to_ascii_lowercase();
            assert!(fleet_scope.contains("fleet_readers"), "{fleet_scope}");
            assert!(fleet_scope.contains("actor"), "{fleet_scope}");
        } else {
            assert!(all_actors.is_none(), "{verb} has no fleet read parameter");
        }
    }
    Ok(())
}

#[tokio::test]
async fn help_propose_params_non_empty_with_title_description_changeset() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "propose").await?;
    let params = schema["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "propose help=true must return non-empty params"
    );
    let has_title = params.iter().any(|p| p["name"] == json!("title"));
    assert!(
        has_title,
        "propose params must include 'title'; got: {params:?}"
    );
    let has_description = params.iter().any(|p| p["name"] == json!("description"));
    assert!(
        has_description,
        "propose params must include 'description'; got: {params:?}"
    );
    let has_changeset = params.iter().any(|p| p["name"] == json!("changeset"));
    assert!(
        has_changeset,
        "propose params must include 'changeset'; got: {params:?}"
    );
    Ok(())
}
