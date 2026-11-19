/// #1789: Agent presentation must retain the stable offset/cursor page keys
/// even when the page is empty or terminal.
#[tokio::test]
async fn list_empty_pages_keep_structural_keys_in_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    let offset_page = agent_one(&client, r#"list(kind="entity", limit=10)"#).await?;
    assert_eq!(offset_page["items"], json!([]));
    assert!(
        offset_page.get("items").is_some(),
        "empty offset page must retain items: {offset_page}"
    );

    let cursor_page = agent_one(&client, r#"list(kind="entity", limit=10, after="")"#).await?;
    assert_eq!(cursor_page["entities"], json!([]));
    assert!(
        cursor_page.get("entities").is_some(),
        "empty cursor page must retain entities: {cursor_page}"
    );
    assert!(
        cursor_page.get("next_after").is_some_and(Value::is_null),
        "terminal cursor page must retain next_after:null: {cursor_page}"
    );
    Ok(())
}

/// Keyset cursor pages from `knowledge.list(after=…)` keep their completion
/// signals under the default Agent presentation: an empty `results` page and
/// `next_after: null` are how a walk terminates (ADR-045 Amendment 4).
#[tokio::test]
async fn knowledge_list_empty_cursor_page_keeps_completion_signals_in_agent_mode(
) -> anyhow::Result<()> {
    let client = connect_knowledge().await?;

    let page = agent_one(&client, r#"knowledge.list(after="", limit=10)"#).await?;
    assert_eq!(page["results"], json!([]));
    assert!(
        page.get("results").is_some(),
        "empty cursor page must retain results: {page}"
    );
    assert!(
        page.get("next_after").is_some_and(Value::is_null),
        "terminal cursor page must retain next_after:null: {page}"
    );
    Ok(())
}

#[cfg(unix)]
async fn seeded_read_only_snapshot_server() -> (tempfile::TempDir, KhiveMcpServer) {
    use std::os::unix::fs::PermissionsExt;

    use khive_mcp::tools::request::RequestParams;

    disable_daemon();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("read_only_snapshot.db");
    let config = RuntimeConfig {
        db_path: Some(path.clone()),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };

    {
        let runtime = KhiveRuntime::new(config.clone()).expect("writable snapshot source");
        let server = KhiveMcpServer::new(runtime).expect("writable server");
        let seeded = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: r#"create(kind="concept", name="snapshot entity")"#.to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
                save_to: None,
                format: Some("json".to_string()),
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("seed dispatch");
        let seeded: Value = serde_json::from_str(&seeded).expect("seed response JSON");
        assert_eq!(seeded["results"][0]["ok"], json!(true), "{seeded}");
    }

    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).unwrap();
    khive_storage::test_support::freeze_snapshot_sidecars(&path);

    let runtime = KhiveRuntime::new(config)
        .expect("normal boot must detect and validate the read-only snapshot");
    assert!(runtime.is_read_only());
    let server = KhiveMcpServer::new(runtime).expect("read-only server builds");
    (dir, server)
}

/// Issue #1602: normal runtime boot detects a chmod-read-only SQLite snapshot,
/// keeps assertive verbs usable, and surfaces the deliberately skipped audit
/// append beside each canonical result. The same backend still rejects writes.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn chmod_read_only_snapshot_serves_stats_and_clamped_list_with_audit_advisory() {
    use khive_mcp::tools::request::RequestParams;

    let (_dir, server) = seeded_read_only_snapshot_server().await;

    let reads = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"[stats(), list(kind="entity", limit=501)]"#.to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("read dispatch");
    let reads: Value = serde_json::from_str(&reads).expect("read response JSON");
    let results = reads["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["ok"], json!(true), "stats must succeed: {reads}");
    assert_eq!(results[0]["result"]["entities"], json!(1));
    assert_eq!(results[1]["ok"], json!(true), "list must succeed: {reads}");
    assert!(
        results[1]["result"]["items"].is_array(),
        "list's stable envelope must be preserved: {}",
        results[1]
    );
    assert_eq!(results[1]["result"]["requested_limit"], json!(501));
    assert_eq!(results[1]["result"]["effective_limit"], json!(500));
    assert_eq!(results[1]["result"]["limit_clamped"], json!(true));
    for entry in results {
        assert_eq!(
            entry["advisories"][0]["code"],
            json!(khive_runtime::AUDIT_PERSISTENCE_SKIPPED_READ_ONLY),
            "successful read must surface the skipped audit write: {entry}"
        );
        assert!(
            entry["result"].get("advisories").is_none(),
            "advisory belongs beside the result, not inside its verb-owned shape"
        );
    }

    let mutation = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"create(kind="concept", name="must fail")"#.to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("mutation returns a per-op error envelope");
    let mutation: Value = serde_json::from_str(&mutation).expect("mutation response JSON");
    assert_eq!(mutation["results"][0]["ok"], json!(false));
    let mutation_error = mutation["results"][0]["error"]
        .to_string()
        .to_ascii_lowercase();
    assert!(
        mutation_error.contains("read-only") || mutation_error.contains("readonly"),
        "mutating verbs must remain rejected: {mutation}"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn chmod_read_only_snapshot_default_list_keeps_items_envelope_and_sibling_audit_advisory() {
    use khive_mcp::tools::request::RequestParams;

    let (_dir, server) = seeded_read_only_snapshot_server().await;
    let response = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"list(kind="entity")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("default-presentation read dispatch");
    let response: Value = serde_json::from_str(&response).expect("read response JSON");
    let entry = &response["results"][0];

    assert_eq!(entry["ok"], json!(true), "list must succeed: {response}");
    assert_eq!(entry["tool"], json!("list"));
    let items = entry["result"]["items"]
        .as_array()
        .expect("offset-mode list must always return the stable items envelope");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], json!("snapshot entity"));
    assert_eq!(items[0]["id"].as_str().map(str::len), Some(8));
    assert_eq!(entry["result"]["limit_clamped"], json!(false));
    assert_eq!(
        entry["advisories"][0]["code"],
        json!(khive_runtime::AUDIT_PERSISTENCE_SKIPPED_READ_ONLY)
    );
    assert!(
        entry["result"].get("advisories").is_none(),
        "the transport advisory must remain outside the verb-owned envelope"
    );
    assert_eq!(response["summary"]["succeeded"], json!(1));
    assert_eq!(response["status"], json!("success"));
}

// ── server info / surface shape ──────────────────────────────────────────────

#[tokio::test]
async fn server_info_advertises_request_tool_only() {
    let server = make_server();
    let info = server.get_info();
    assert_eq!(info.server_info.name, "khive-mcp");
    let instructions = info.instructions.unwrap_or_default();
    assert!(
        instructions.contains("request-only"),
        "instructions should explain the request-only surface"
    );
    // Pack verbs must appear in the catalog so agents can discover what's
    // loaded. The catalog is carried once, by the `request` tool description
    // (ADR-016); the instructions name where it is and do not repeat it.
    assert!(
        instructions.contains("The `request` tool's description lists every verb"),
        "instructions should point to the catalog"
    );
    assert!(
        !instructions.contains("\n  create — ") && !instructions.contains("gtd.assign — "),
        "instructions must not repeat the verb catalog: {instructions}"
    );
    let catalog = request_tool_description(make_server())
        .await
        .expect("tools/list answers");
    assert!(
        catalog.contains("\n  gtd.assign — "),
        "gtd verb should appear"
    );
    assert!(catalog.contains("\n  create — "), "kg verb should appear");
}

#[tokio::test]
async fn list_tools_returns_only_request() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client.list_tools(None).await?;
    let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, vec!["request"], "surface should be a single tool");
    Ok(())
}

#[tokio::test]
async fn request_tool_description_contains_dynamic_verb_catalog() -> anyhow::Result<()> {
    let client = connect().await?;
    let listed = client.list_tools(None).await?;
    let request = listed
        .tools
        .iter()
        .find(|t| t.name == "request")
        .expect("request tool must be present");
    let desc = request.description.as_deref().unwrap_or("");

    // The dynamic catalog must reach `tools/list` consumers (ADR-027). Each
    // verb the kg pack registers should appear by name in the description.
    for verb in [
        "create",
        "get",
        "list",
        "update",
        "delete",
        "merge",
        "search",
        "link",
        "neighbors",
        "traverse",
        "query",
    ] {
        assert!(
            desc.contains(verb),
            "request description missing verb {verb:?}: {desc}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn request_tool_description_declares_comm_read_dependency_contract() -> anyhow::Result<()> {
    let client = connect().await?;
    let listed = client.list_tools(None).await?;
    let request = listed
        .tools
        .iter()
        .find(|t| t.name == "request")
        .expect("request tool must be present");
    let desc = request.description.as_deref().unwrap_or("");

    assert!(
        desc.contains("comm.read") && desc.contains("comm.mark_read"),
        "the request surface must name state-mutating read acknowledgements: {desc}"
    );
    assert!(
        desc.contains("does not wait for or depend on comm.send/comm.reply"),
        "the parallel sibling-independence hazard must be explicit: {desc}"
    );
    assert!(
        desc.contains("comm.reply delivers first") && desc.contains("use a chain"),
        "the contract must give both safe sequencing alternatives: {desc}"
    );
    Ok(())
}
