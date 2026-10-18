// ── help=true schema envelopes for comm + schedule verbs ──────────────────────

fn make_comm_schedule_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime with comm+schedule");
    KhiveMcpServer::new(runtime).expect("server builds with kg+comm+schedule")
}

async fn connect_comm_schedule(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_comm_schedule_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `comm.send(help=true)` must return a non-empty params array with required `to` and `content`.
#[tokio::test]
async fn send_help_returns_required_to_and_content() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "comm.send(help=true)").await?;

    assert_eq!(result["verb"], "comm.send");
    assert_eq!(result["pack"], "comm");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "send help must have non-empty params");

    let to = params
        .iter()
        .find(|p| p["name"] == "to")
        .expect("send help must include 'to'");
    assert_eq!(to["required"], serde_json::json!(true));

    let content = params
        .iter()
        .find(|p| p["name"] == "content")
        .expect("send help must include 'content'");
    assert_eq!(content["required"], serde_json::json!(true));

    Ok(())
}

/// `comm.inbox(help=true)` must advertise every optional pagination/filter parameter.
#[tokio::test]
async fn inbox_help_returns_optional_pagination_and_filters() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "comm.inbox(help=true)").await?;

    assert_eq!(result["verb"], "comm.inbox");
    assert_eq!(result["pack"], "comm");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "inbox help must have non-empty params");

    for name in [
        "limit",
        "offset",
        "status",
        "wait_ms",
        "from_actor",
        "from_prefix",
        "exclude_from_actor",
        "since",
        "before",
        "subject_contains",
        "content_contains",
    ] {
        let param = params
            .iter()
            .find(|param| param["name"] == name)
            .unwrap_or_else(|| panic!("inbox help must include {name:?}"));
        assert_eq!(param["required"], serde_json::json!(false));
    }

    Ok(())
}

/// `comm.read(help=true)` advertises the exactly-one-of `id`/`ids` bulk surface.
#[tokio::test]
async fn read_help_returns_optional_id_and_ids() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "comm.read(help=true)").await?;
    let params = result["params"]
        .as_array()
        .expect("params must be an array");

    for name in ["id", "ids"] {
        let param = params
            .iter()
            .find(|param| param["name"] == name)
            .unwrap_or_else(|| panic!("read help must include {name:?}"));
        assert_eq!(param["required"], serde_json::json!(false));
    }

    Ok(())
}

/// `comm.mark_read(help=true)` names the mutation and exposes its atomic opt-in.
#[tokio::test]
async fn mark_read_help_distinguishes_marking_from_retrieval() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "comm.mark_read(help=true)").await?;

    assert_eq!(result["verb"], "comm.mark_read");
    assert!(result["description"]
        .as_str()
        .unwrap()
        .contains("comm.inbox"));
    assert!(result["description"]
        .as_str()
        .unwrap()
        .contains("comm.thread"));

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    let ids = params
        .iter()
        .find(|param| param["name"] == "ids")
        .expect("mark_read help must include ids");
    assert_eq!(ids["required"], serde_json::json!(true));
    let atomic = params
        .iter()
        .find(|param| param["name"] == "atomic")
        .expect("mark_read help must include atomic");
    assert_eq!(atomic["required"], serde_json::json!(false));

    Ok(())
}

/// `schedule.schedule(help=true)` must return required `action` and `at`.
#[tokio::test]
async fn schedule_help_returns_required_action_and_at() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "schedule.schedule(help=true)").await?;

    assert_eq!(result["verb"], "schedule.schedule");
    assert_eq!(result["pack"], "schedule");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "schedule help must have non-empty params"
    );

    let action = params
        .iter()
        .find(|p| p["name"] == "action")
        .expect("schedule help must include 'action'");
    assert_eq!(action["required"], serde_json::json!(true));

    let at = params
        .iter()
        .find(|p| p["name"] == "at")
        .expect("schedule help must include 'at'");
    assert_eq!(at["required"], serde_json::json!(true));

    Ok(())
}

/// `schedule.remind(help=true)` must return required `content` and `at`, optional `repeat`.
#[tokio::test]
async fn remind_help_returns_required_content_and_at() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "schedule.remind(help=true)").await?;

    assert_eq!(result["verb"], "schedule.remind");
    assert_eq!(result["pack"], "schedule");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "remind help must have non-empty params");

    let content = params
        .iter()
        .find(|p| p["name"] == "content")
        .expect("remind help must include 'content'");
    assert_eq!(content["required"], serde_json::json!(true));

    let at = params
        .iter()
        .find(|p| p["name"] == "at")
        .expect("remind help must include 'at'");
    assert_eq!(at["required"], serde_json::json!(true));

    let repeat = params
        .iter()
        .find(|p| p["name"] == "repeat")
        .expect("remind help must include 'repeat'");
    assert_eq!(repeat["required"], serde_json::json!(false));

    Ok(())
}
