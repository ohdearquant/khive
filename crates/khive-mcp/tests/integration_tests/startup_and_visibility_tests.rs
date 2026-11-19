// ── Fix 1: run_migrations() at MCP startup ──────────────────────────────────

/// V15 (`proposals_open`) and V16/V17 (vec `embedding_model` column) are
/// applied by `KhiveRuntime::new` before any pack handler runs.  Without the
/// fix, `propose(...)` fails with "no such table: proposals_open" on a fresh
/// file-backed database.
///
/// This test creates a fresh tempfile-backed runtime (the path is not
/// pre-migrated), creates a `propose` op, and asserts it succeeds — proving
/// the migration ran at construction time.
#[tokio::test]
async fn startup_migrations_applied_to_fresh_file_backed_db() -> anyhow::Result<()> {
    // Required for correctness under process-per-test runners: without it this
    // test only passed when a sibling test in the same process had already set
    // KHIVE_NO_DAEMON, and a failed daemon spawn surfaces as `respawn_failed`
    // with no local-dispatch fallback (ADR-049 Amendment 2).
    disable_daemon();
    let db_file = tempfile::NamedTempFile::new()?;
    let config = RuntimeConfig {
        db_path: Some(db_file.path().to_path_buf()),
        default_namespace: Namespace::parse("fix1test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("fresh file-backed runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds");

    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    // First create an entity to propose a change against.
    let entity = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="MigrationTarget")"#,
    )
    .await?;
    // Entity create in verbose mode returns `id` (full UUID), not `full_id`.
    let eid = entity["id"].as_str().unwrap().to_string();

    // `propose` writes to proposals_open (V15). Before the fix this would
    // crash with "no such table: proposals_open" on a fresh DB.
    //
    // Use the JSON batch form to pass the nested changeset without DSL quoting
    // issues — the JSON form is equivalent per ADR-016 §§.
    let ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "migration regression test",
            "description": "fix1: run_migrations at startup",
            "changeset": {
                "kind": "add_entity",
                "entity": {
                    "kind": "concept",
                    "name": format!("fix1-{eid}")
                }
            }
        }
    }]))
    .unwrap();
    let result = call(
        &client,
        "request",
        json!({
            "ops": ops,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"], true,
        "propose must succeed on a freshly-migrated DB; got: {first}"
    );
    Ok(())
}

// ── Fix 2: Visibility::Subhandler gate ──────────────────────────────────────

/// `brain.state`, `brain.config`, `brain.events`, and `brain.emit` are
/// tagged `Visibility::Subhandler` in the brain pack.  The MCP request
/// surface must reject them with a per-op `{ok: false}` rather than routing
/// to the handler.  `help=true` introspection must still work (short-circuit
/// before the gate).
fn make_brain_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("braintest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "brain".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+brain runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+brain")
}

#[tokio::test]
async fn subhandler_verbs_are_blocked_at_mcp_boundary() -> anyhow::Result<()> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_brain_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    // All four Subhandler verbs must be rejected.
    for verb in &["brain.state", "brain.config", "brain.events", "brain.emit"] {
        let result = call(&client, "request", json!({"ops": format!("{verb}()")})).await?;
        let body: Value = serde_json::from_str(&first_text(&result))?;
        let first = &body["results"][0];
        assert_eq!(
            first["ok"], false,
            "Subhandler verb {verb:?} must be blocked: got {first}"
        );
        let err = first["error"]["message"].as_str().unwrap_or("");
        assert!(
            err.contains("permission denied") || err.contains("subhandler"),
            "error for {verb:?} must mention permission/subhandler: {err}"
        );
    }
    Ok(())
}

/// The operator path (`dispatch_request_local`, the path `kkernel exec` uses)
/// must NOT gate subhandler verbs. The visibility gate is wire-only
/// (`from_wire`): agents cannot reach subhandlers via the MCP `request` tool,
/// but operators invoke them directly. This is the invariant that regressed
/// when the gate lived in the shared dispatch — every handler had to be
/// promoted to `Verb` to stay reachable, which is exactly what we are undoing.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn subhandler_verbs_are_allowed_on_operator_path() -> anyhow::Result<()> {
    use khive_mcp::tools::request::RequestParams;

    let server = make_brain_server();

    for verb in &["brain.state", "brain.config", "brain.events"] {
        let raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: format!("{verb}()"),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("operator dispatch must not RPC-fail");
        let body: Value = serde_json::from_str(&raw)?;
        let first = &body["results"][0];
        let err = first["error"]["message"].as_str().unwrap_or("");
        // The gate must NOT have fired: its signature message must be absent.
        // The handler may still succeed (ok=true) or fail for its own reasons,
        // but it must have been *reached*, not blocked at the visibility gate.
        assert!(
            !err.contains("internal subhandler") && !err.contains("permission denied"),
            "operator path must NOT gate subhandler {verb:?}: {first}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn subhandler_verb_help_introspection_still_works() -> anyhow::Result<()> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_brain_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    // `help=true` is short-circuited before the visibility gate — must succeed.
    let result = ok_one(&client, r#"brain.state(help=true)"#).await?;
    // Help response includes the verb name or param list.
    let text = serde_json::to_string(&result).unwrap_or_default();
    assert!(
        text.contains("brain.state") || text.contains("params") || text.contains("help"),
        "help response for Subhandler verb must return introspection data: {text}"
    );
    Ok(())
}
