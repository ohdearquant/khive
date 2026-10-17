// ── MCP-wide ISO-8601 timestamps ──────────────────────────────────────────

fn assert_agent_created_at_pair(
    agent_row: &Value,
    canonical_row: &Value,
    request_started: chrono::DateTime<chrono::Utc>,
    response_received: chrono::DateTime<chrono::Utc>,
) {
    let canonical = canonical_row["created_at"]
        .as_str()
        .expect("Verbose row must carry created_at");
    let instant = chrono::DateTime::parse_from_rfc3339(canonical)
        .expect("canonical created_at must parse")
        .with_timezone(&chrono::Utc);
    assert!(canonical.ends_with('Z'), "the fixture producer emits UTC Z");
    assert_eq!(
        agent_row["created_at"], canonical_row["created_at"],
        "Agent row must retain the exact canonical instant and fraction"
    );
    assert!(
        canonical_row.get("created_at_relative").is_none(),
        "the relative sibling must be added by Agent presentation"
    );

    let relative = agent_row["created_at_relative"]
        .as_str()
        .expect("Agent list row must carry created_at_relative");
    let token = relative
        .strip_suffix(" ago")
        .expect("relative timestamp must end with ' ago'");
    let (digits, unit) = token.split_at(token.len() - 1);
    let count: u64 = digits.parse().expect("relative count must be decimal");
    let seconds_per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => panic!("unexpected relative unit: {relative}"),
    };
    let age_at_start = (request_started.timestamp() - instant.timestamp()).max(0) as u64;
    let age_at_end = (response_received.timestamp() - instant.timestamp()).max(0) as u64;
    // Integer-second sampling can cross a boundary around a microsecond fixture.
    assert!(
        count.saturating_mul(seconds_per_unit) <= age_at_end.saturating_add(1)
            && count.saturating_add(1).saturating_mul(seconds_per_unit)
                > age_at_start.saturating_sub(1),
        "{relative} must describe the row age during the MCP call ({age_at_start}..={age_at_end}s)"
    );
    if age_at_end.saturating_add(1) < 60 {
        assert_eq!(unit, "s", "fresh rows use the seconds band");
    }
    assert!(agent_row.get("updated_at_relative").is_none());
}

/// `remember` must return ISO-8601 `created_at` (not a raw microsecond i64).
#[tokio::test]
async fn remember_returns_iso8601_timestamp() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = ok_one(
        &client,
        r#"memory.remember(content="r3 timestamp test", salience=0.7)"#,
    )
    .await?;

    let created_at = result["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "remember created_at must be ISO-8601 string, got: {:?}",
        result["created_at"]
    );
    Ok(())
}

/// `recall` must return ISO-8601 `created_at` on each hit (not raw i64).
#[tokio::test]
async fn recall_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_full().await?;

    // Seed a memory first.
    ok_one(
        &client,
        r#"memory.remember(content="r3 recall timestamp seed")"#,
    )
    .await?;

    let result = ok_one(
        &client,
        r#"memory.recall(query="r3 recall timestamp seed", limit=1)"#,
    )
    .await?;

    let hits = result.as_array().expect("recall returns array");
    assert!(!hits.is_empty(), "recall must return at least one hit");
    let created_at = hits[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "recall hit created_at must be ISO-8601 string, got: {:?}",
        hits[0]["created_at"]
    );
    Ok(())
}

#[tokio::test]
async fn agent_note_list_created_at_has_exact_and_relative_forms() -> anyhow::Result<()> {
    let client = connect().await?;
    let protected = "2026-09-29T19:18:25.132811+03:00";
    let created = ok_one(
        &client,
        &format!(
            "create(kind=\"observation\", content=\"am9 note list timestamp\", properties={{\"source_at\":{}}})",
            json!(protected)
        ),
    )
    .await?;

    let request_started = chrono::Utc::now();
    let agent = agent_one(&client, r#"list(kind="note", limit=1)"#).await?;
    let response_received = chrono::Utc::now();
    let canonical = ok_one(&client, r#"list(kind="note", limit=1)"#).await?;
    let agent_row = &agent["items"][0];
    let canonical_row = &canonical["items"][0];
    assert_eq!(canonical_row["id"], created["id"]);
    assert_agent_created_at_pair(agent_row, canonical_row, request_started, response_received);
    assert_eq!(agent_row["updated_at"], canonical_row["updated_at"]);
    assert_eq!(agent_row["properties"]["source_at"], protected);
    assert_eq!(
        agent_row["properties"]["source_at"],
        canonical_row["properties"]["source_at"]
    );

    let root = agent_one(&client, &format!("get(id={})", json!(created["id"]))).await?;
    assert_eq!(root["created_at"], canonical_row["created_at"]);
    assert!(root.get("created_at_relative").is_none());
    Ok(())
}

#[tokio::test]
async fn agent_memory_recall_created_at_has_exact_and_relative_forms() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let ops = r#"memory.recall(query="am9 memory timestamp witness", limit=1, fusion_strategy="keyword_only", min_score=0.0)"#;
    ok_one(
        &client,
        r#"memory.remember(content="am9 memory timestamp witness")"#,
    )
    .await?;

    let request_started = chrono::Utc::now();
    let agent = agent_one(&client, ops).await?;
    let response_received = chrono::Utc::now();
    let canonical = ok_one(&client, ops).await?;
    let agent_row = agent
        .as_array()
        .and_then(|rows| rows.first())
        .expect("Agent recall must return the seeded memory");
    let canonical_row = canonical
        .as_array()
        .and_then(|rows| rows.first())
        .expect("Verbose recall must return the seeded memory");
    assert_eq!(agent_row["full_id"], canonical_row["full_id"]);
    assert_agent_created_at_pair(agent_row, canonical_row, request_started, response_received);
    Ok(())
}

fn make_comm_server_only() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("commtest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+comm runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+comm")
}

async fn connect_comm_only(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_comm_server_only();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

#[tokio::test]
async fn comm_strict_identifiers_round_trip_in_agent_mode() -> anyhow::Result<()> {
    let client = connect_comm_only().await?;
    let sent = agent_one(
        &client,
        r#"comm.send(to="commtest", content="strict root")"#,
    )
    .await?;
    let thread_id = sent["thread_id"]
        .as_str()
        .expect("send must return its canonical thread root");
    assert_eq!(thread_id.len(), 36, "thread_id must not be shortened");

    agent_one(
        &client,
        &format!(r#"comm.send(to="commtest", content="same thread", thread_id="{thread_id}")"#),
    )
    .await?;

    let outbound_id = sent["full_id"]
        .as_str()
        .expect("send must return its full outbound id");
    let delivered = agent_one(&client, &format!(r#"comm.delivered(id="{outbound_id}")"#)).await?;
    let returned = delivered["id"]
        .as_str()
        .expect("delivery confirmation must echo its correlation id");
    assert_eq!(returned, outbound_id);
    assert_eq!(returned.len(), 36, "delivery correlation id must stay full");

    agent_one(&client, &format!(r#"comm.delivered(id="{returned}")"#)).await?;
    Ok(())
}

/// `inbox` returns message notes; `created_at` and `updated_at` must be ISO-8601
/// strings (not raw microsecond i64s) — validates note_to_message_json fix.
#[tokio::test]
async fn send_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_comm_only().await?;

    // Self-send produces one outbound note visible to inbox.
    ok_one(
        &client,
        r#"comm.send(to="commtest", content="r3 timestamp test message")"#,
    )
    .await?;

    // inbox lists inbound; self-send is the same note — use the listing path
    // that calls note_to_message_json which we fixed.
    let result = call(
        &client,
        "request",
        json!({"ops": r#"list(kind="note", limit=1)"#, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "list(kind=note) must succeed: {first}"
    );
    let items = first["result"]["items"]
        .as_array()
        .expect("list returns an items envelope");
    assert!(!items.is_empty(), "must have at least one message note");
    let created_at = items[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "message note created_at must be ISO-8601 string, got: {:?}",
        items[0]["created_at"]
    );
    Ok(())
}

#[tokio::test]
async fn agent_comm_inbox_created_at_has_exact_and_relative_forms() -> anyhow::Result<()> {
    let client = connect_comm_only().await?;
    ok_one(
        &client,
        r#"comm.send(to="commtest", content="am9 sent mailbox timestamp witness")"#,
    )
    .await?;

    let ops = r#"comm.inbox(box="sent", limit=1)"#;
    let request_started = chrono::Utc::now();
    let agent = agent_one(&client, ops).await?;
    let response_received = chrono::Utc::now();
    let canonical = ok_one(&client, ops).await?;
    let agent_row = &agent["messages"][0];
    let canonical_row = &canonical["messages"][0];
    assert_eq!(agent_row["full_id"], canonical_row["full_id"]);
    assert_agent_created_at_pair(agent_row, canonical_row, request_started, response_received);
    assert_eq!(agent_row["updated_at"], canonical_row["updated_at"]);
    assert_eq!(
        agent_row["properties"]["sent_at"],
        canonical_row["properties"]["sent_at"]
    );
    assert!(agent_row["properties"]["sent_at"].is_string());
    Ok(())
}

fn make_schedule_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("schedtest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+comm+schedule runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+comm+schedule")
}

async fn connect_schedule(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_schedule_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `remind` creates a scheduled_event note; `agenda` returns ISO-8601 timestamps.
#[tokio::test]
async fn agenda_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_schedule().await?;

    ok_one(
        &client,
        r#"schedule.remind(content="r3 agenda ts test", at="2099-01-01T00:00:00Z")"#,
    )
    .await?;

    let result = ok_one(&client, r#"schedule.agenda()"#).await?;
    // agenda returns { events: [...], count: N }
    let items = result["events"]
        .as_array()
        .expect("agenda returns events array");
    assert!(!items.is_empty(), "agenda must return at least one event");
    let created_at = items[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "agenda event created_at must be ISO-8601 string, got: {:?}",
        items[0]["created_at"]
    );
    Ok(())
}

async fn connect_brain_only(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    disable_daemon();
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let config = RuntimeConfig {
        db_path: None,
        actor_id: Some("brain-feedback-test".to_string()),
        default_namespace: Namespace::parse("braintest2").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "brain".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+brain runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds");
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `brain.profiles` must return ISO-8601 `created_at` on profile records.
#[tokio::test]
async fn brain_profiles_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_brain_only().await?;

    let result = ok_one(&client, r#"brain.profiles()"#).await?;
    let profiles = result["profiles"]
        .as_array()
        .expect("brain.profiles returns profiles array");
    assert!(
        !profiles.is_empty(),
        "brain.profiles must return at least one profile"
    );
    let created_at = profiles[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "brain.profiles created_at must be ISO-8601 string, got: {:?}",
        profiles[0]["created_at"]
    );
    Ok(())
}

/// `propose` + `list(kind="proposal")` must return ISO-8601 timestamps on proposal rows.
#[tokio::test]
async fn proposal_list_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    ok_one(
        &client,
        r#"propose(title="r3 ts test proposal", description="r3 timestamp regression test", changeset={"kind": "add_entity", "entity": {"kind": "concept", "name": "R3TsEntity"}})"#,
    )
    .await?;

    let result = ok_one(&client, r#"list(kind="proposal")"#).await?;
    let proposals = result["items"]
        .as_array()
        .expect("list(kind=proposal) returns an items envelope");
    assert!(!proposals.is_empty(), "must have at least one proposal");
    let created_at = proposals[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "proposal list created_at must be ISO-8601 string, got: {:?}",
        proposals[0]["created_at"]
    );
    Ok(())
}
