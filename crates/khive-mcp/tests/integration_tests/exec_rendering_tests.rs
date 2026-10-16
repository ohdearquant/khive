// ── #81: exec JSON output valid when content contains backslash escapes ──────

/// Regression for #81: note content containing backslash escape sequences
/// (`\n`, `\\`, `\t`, `\"`) must produce valid, parseable JSON from
/// `dispatch_request_local` — the path exercised by `kkernel exec`.
///
/// Previously, a stale daemon binary (pre-c5ffc54) had a code path that
/// interpolated result strings into JSON without going through serde, causing
/// `Invalid \escape` parse errors at the caller. This test locks the correct
/// behavior: create → output parses clean; update → output parses clean;
/// content round-trips byte-identical.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn exec_output_valid_json_with_backslash_escape_content() -> anyhow::Result<()> {
    use khive_mcp::tools::request::RequestParams;

    let server = {
        std::env::set_var("KHIVE_NO_DAEMON", "1");
        let config = khive_runtime::RuntimeConfig {
            db_path: None,
            default_namespace: khive_runtime::Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..khive_runtime::RuntimeConfig::default()
        };
        let runtime = khive_runtime::KhiveRuntime::new(config).expect("in-memory runtime");
        khive_mcp::server::KhiveMcpServer::new(runtime).expect("server builds")
    };

    // Content with every common backslash-escape type: newline, tab, backslash,
    // embedded quote. The DSL arg is a JSON-quoted string so the parser sees
    // the escape sequences and serde_json decodes them to the real characters.
    let content_with_escapes = "line1\nline2\t\\tabbed\\ \"quoted\"";
    let create_ops = format!(
        r#"create(kind="observation", content="{}")"#,
        // Escape for the DSL/JSON string: newline → \n, tab → \t, backslash → \\, quote → \"
        content_with_escapes
            .replace('\\', r"\\")
            .replace('"', r#"\""#)
            .replace('\n', r"\n")
            .replace('\t', r"\t")
    );

    // ── Step 1: create — output must be valid JSON ────────────────────────────
    let create_out = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: create_ops,
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("dispatch must succeed");

    let create_body: Value = serde_json::from_str(&create_out)
        .expect("#81 regression: create output must be valid JSON");
    let first = &create_body["results"][0];
    assert_eq!(first["ok"], json!(true), "create must succeed: {first}");
    let note_id = first["result"]["id"].as_str().expect("id present");

    // ── Step 2: get — content round-trips byte-identical ─────────────────────
    let get_out = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: format!(r#"get(id="{note_id}")"#),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("get dispatch must succeed");

    let get_body: Value =
        serde_json::from_str(&get_out).expect("#81 regression: get output must be valid JSON");
    let get_first = &get_body["results"][0];
    assert_eq!(
        get_first["ok"],
        json!(true),
        "get must succeed: {get_first}"
    );
    let got_content = get_first["result"]["content"]
        .as_str()
        .expect("content field present");
    assert_eq!(
        got_content, content_with_escapes,
        "#81 regression: content with backslash escapes must round-trip byte-identical"
    );

    // ── Step 3: update with new backslash content — output valid JSON ─────────
    let updated_content = "updated\\npath\\t\"value\"";
    let update_ops = format!(
        r#"update(id="{note_id}", content="{}")"#,
        updated_content
            .replace('\\', r"\\")
            .replace('"', r#"\""#)
            .replace('\n', r"\n")
            .replace('\t', r"\t")
    );
    let update_out = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: update_ops,
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("update dispatch must succeed");

    let update_body: Value = serde_json::from_str(&update_out)
        .expect("#81 regression: update output must be valid JSON");
    let update_first = &update_body["results"][0];
    assert_eq!(
        update_first["ok"],
        json!(true),
        "update must succeed: {update_first}"
    );

    Ok(())
}
