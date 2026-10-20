#[tokio::test]
async fn pack_only_kg_omits_gtd_verbs_from_catalog() {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let catalog = request_tool_description(server)
        .await
        .expect("tools/list answers");
    assert!(catalog.contains("\n  create — "), "kg verb missing");
    assert!(
        !catalog.contains("gtd.assign"),
        "gtd verb should not be in catalog when only kg is loaded"
    );
}

#[tokio::test]
async fn pack_gtd_without_kg_fails_at_boot() {
    // ADR-027: gtd declares requires=["kg"]; omitting "kg" from the pack list
    // must fail at boot with a clear error — not silently auto-add kg.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    match KhiveMcpServer::new(runtime) {
        Ok(_) => panic!("gtd without kg must fail: missing dependency is a boot error (ADR-027)"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("kg") || msg.contains("unknown pack"),
                "error must name the missing dependency: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn pack_schedule_without_comm_rejects_only_remind_before_persisting() -> anyhow::Result<()> {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+schedule runtime");
    let server = KhiveMcpServer::new(runtime).expect("kg+schedule server builds without comm");
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"schedule.remind(content="must not persist", at="2099-01-01T00:00:00Z")"#,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let failed = &body["results"][0];
    assert_eq!(failed["ok"], json!(false), "remind must fail: {failed}");
    let error = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        error.contains("comm.send") && error.contains("delivery"),
        "error must name the missing comm delivery capability: {error}"
    );

    let notes = ok_one(&client, r#"list(kind="note")"#).await?;
    assert_eq!(
        notes["items"],
        json!([]),
        "failed remind must persist no note"
    );
    let empty_agenda = ok_one(&client, "schedule.agenda()").await?;
    assert_eq!(empty_agenda["count"], json!(0));

    let scheduled = ok_one(
        &client,
        r#"schedule.schedule(action="schedule.agenda()", at="2099-01-02T00:00:00Z")"#,
    )
    .await?;
    let full_id = scheduled["full_id"]
        .as_str()
        .expect("schedule.schedule returns full_id");
    let agenda = ok_one(&client, "schedule.agenda()").await?;
    assert_eq!(agenda["count"], json!(1));
    let cancelled = ok_one(&client, &format!(r#"schedule.cancel(id="{full_id}")"#)).await?;
    assert_eq!(cancelled["status"], json!("cancelled"));

    Ok(())
}

#[tokio::test]
async fn pack_gtd_with_kg_explicit_works() {
    // When both kg and gtd are listed, gtd's requires=["kg"] is satisfied.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime).expect("kg+gtd builds");
    let catalog = request_tool_description(server)
        .await
        .expect("tools/list answers");
    assert!(
        catalog.contains("\n  gtd.assign — "),
        "gtd verb must be present"
    );
    assert!(catalog.contains("\n  create — "), "kg verb must be present");
}

#[tokio::test]
async fn json_form_request_works_identically() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"[{"tool":"gtd.assign","args":{"title":"json form","priority":"p1"}}]"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["succeeded"], 1);
    assert_eq!(body["results"][0]["result"]["title"], "json form");
    Ok(())
}

/// RUNTIME-AUD-002 (#433): a present-but-malformed `namespace` value entering
/// through the MCP JSON-form wire boundary must fail closed with a per-op
/// invalid-input error — never be silently coerced to the default namespace and
/// written. The JSON parser preserves the non-string value verbatim (it is not
/// dropped or treated as absent), so `VerbRegistry::dispatch` sees it and
/// rejects it before any gate Allow / storage write. This is the end-to-end
/// ingress counterpart to the runtime-layer `namespace_null_rejected_not_coerced`
/// gate-spy test.
#[tokio::test]
async fn json_form_namespace_non_string_returns_invalid_input() -> anyhow::Result<()> {
    let client = connect().await?;

    // Every non-string JSON type the finding enumerates, embedded in a real
    // JSON-form `create` op exactly as an MCP client would send it.
    let cases: [(&str, &str); 5] = [
        ("null", "null"),
        ("number", "42"),
        ("boolean", "true"),
        ("array", r#"["local"]"#),
        ("object", r#"{"ns":"local"}"#),
    ];

    for (label, ns_json) in cases {
        let ops = format!(
            r#"[{{"tool":"create","args":{{"kind":"entity","entity_kind":"concept","name":"aud002-{label}","namespace":{ns_json}}}}}]"#
        );
        let result = call(&client, "request", json!({ "ops": ops })).await?;
        let body: Value = serde_json::from_str(&first_text(&result))?;

        // Malformed namespace is a per-op validation failure, NOT a protocol
        // error — the JSON parses fine, so the batch is not aborted.
        let first = &body["results"][0];
        assert_eq!(
            first["ok"],
            json!(false),
            "case {label}: op must fail closed, got: {body}"
        );
        let err = first["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            err.contains("namespace"),
            "case {label}: error must name the namespace, got: {first}"
        );

        // No local write may have slipped through under a coerced default.
        assert_eq!(
            body["summary"]["succeeded"], 0,
            "case {label}: no op may succeed, got: {body}"
        );
        assert_eq!(
            body["summary"]["failed"], 1,
            "case {label}: the malformed op must be counted as failed, got: {body}"
        );
    }

    Ok(())
}
