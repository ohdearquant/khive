// ── ADR-078 server-level format tests (server seam coverage) ────────────────
//
// These tests drive `dispatch_request_local` directly so they pin the PUBLIC
// server behaviour that combines `format`, `format_per_op`,
// `presentation_per_op`, ok entries, and error entries.

fn make_format_server() -> KhiveMcpServer {
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let config = khive_runtime::RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..khive_runtime::RuntimeConfig::default()
    };
    let rt = khive_runtime::KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(rt).expect("server builds")
}

/// (fmt-1) Mixed ok/error batch under `format=auto`: error entries must always
/// be compact JSON; ok entries must be formatted with the requested format.
///
/// ADR-078 §8.2: error envelopes are never passed through auto/table renderers.
/// ADR-078 §8.4: ok results are rendered per-op.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn format_auto_mixed_ok_error_batch_error_stays_compact() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Batch: op0 succeeds (stats()), op1 fails (bad verb).
    let params = RequestParams {
        plan: None,
        ops: r#"[stats(), no_such_verb()]"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: Some("auto".to_string()),
        format_per_op: None,
        request_id: None,
    };

    let raw = server
        .dispatch_request_local(params)
        .await
        .expect("dispatch must not itself fail");

    // The outer envelope must be valid JSON regardless of format.
    let body: serde_json::Value =
        serde_json::from_str(&raw).expect("envelope must be valid JSON under format=auto");

    let results = body["results"]
        .as_array()
        .expect("results array must be present");
    assert_eq!(results.len(), 2, "batch must produce 2 result entries");

    // op0 succeeded — result should have been formatted (rendered as string).
    assert_eq!(
        results[0]["ok"],
        serde_json::json!(true),
        "op0 (stats) must succeed: {}",
        results[0]
    );

    // op1 failed — error envelope must be compact JSON (never reformatted).
    assert_eq!(
        results[1]["ok"],
        serde_json::json!(false),
        "op1 (no_such_verb) must fail: {}",
        results[1]
    );
    // The structured error and its message must remain JSON, not a rendered table.
    assert!(
        results[1]["error"].is_object() && results[1]["error"]["message"].is_string(),
        "error object must retain its plain message without reformatting: {}",
        results[1]
    );
    // Summary must always be present and valid.
    assert_eq!(body["summary"]["total"], serde_json::json!(2));
}

/// (fmt-2) `format_per_op` overrides: op0 gets json, op1 gets auto.
///
/// Pins ADR-078 §8.4: a single `format` applies uniformly; `format_per_op`
/// overrides per position.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn format_per_op_override_selects_format_per_position() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Create two entities, then list — use gtd.assign so results are non-trivial.
    // First build a state: two assign ops (both json), then one stats op (auto).
    // Simpler: two parallel stats() calls — one forced json, one forced auto.
    let params = RequestParams {
        plan: None,
        ops: r#"[stats(), stats()]"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        // Batch default is auto, but op0 overrides to json.
        format: Some("auto".to_string()),
        format_per_op: Some(vec![
            Some("json".to_string()), // op0 → json (compact, parseable)
            None,                     // op1 → inherits batch "auto"
        ]),
        request_id: None,
    };

    let raw = server
        .dispatch_request_local(params)
        .await
        .expect("dispatch must succeed");

    let body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON envelope");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);

    // op0 was forced json → the result stays as the original JSON Value (not
    // wrapped as a string).  When format=json, render_result passes the entry
    // through unchanged (the entry.result remains a JSON object, not a string).
    assert_eq!(results[0]["ok"], serde_json::json!(true));
    let op0_result = &results[0]["result"];
    assert!(
        op0_result.is_object(),
        "json-format op result must remain a JSON object (not reformatted to string): {op0_result}"
    );
    // stats() returns entity/edge/note counts — these fields must be present.
    assert!(
        op0_result.get("entities").is_some() || op0_result.get("total").is_some(),
        "op0 json result must contain stats fields: {op0_result}"
    );

    // op1 inherited auto → result is a rendered string (not a raw json object).
    assert_eq!(results[1]["ok"], serde_json::json!(true));
    assert!(
        results[1]["result"].is_string(),
        "auto-format op result must be a rendered string: {}",
        results[1]
    );

    // Summary envelope must be compact JSON regardless of format.
    assert_eq!(body["summary"]["total"], serde_json::json!(2));
    assert_eq!(body["summary"]["succeeded"], serde_json::json!(2));
}

/// Agent JSON applies ADR-078 redundancy reduction while Verbose JSON retains
/// the canonical shape. Chain substitution must continue to read canonical
/// pre-presentation results under both JSON and auto output.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn agent_json_deduplicates_gtd_and_preserves_chain_inputs() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    let agent_raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"gtd.assign(title="agent-json-dedup", priority="p1", assignee="lambda:test")"#
                .to_string(),
            presentation: Some("agent".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("agent/json task creation must succeed");
    let agent_body: serde_json::Value = serde_json::from_str(&agent_raw).unwrap();
    let agent_task = &agent_body["results"][0]["result"];
    for key in ["assignee", "priority", "status"] {
        assert!(agent_task.get(key).is_some(), "top-level {key} must remain");
        assert!(
            agent_task["properties"].get(key).is_none(),
            "agent/json must not duplicate {key} inside properties: {agent_task}"
        );
    }

    let verbose_raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops:
                r#"gtd.assign(title="verbose-json-control", priority="p1", assignee="lambda:test")"#
                    .to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("verbose/json task creation must succeed");
    let verbose_body: serde_json::Value = serde_json::from_str(&verbose_raw).unwrap();
    let verbose_task = &verbose_body["results"][0]["result"];
    for key in ["assignee", "priority", "status"] {
        assert_eq!(
            verbose_task.get(key),
            verbose_task["properties"].get(key),
            "verbose/json must retain canonical duplicate {key}: {verbose_task}"
        );
    }

    let target_raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"create(kind="entity", entity_kind="concept", name="AgentJsonChainTarget")"#
                .to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("chain target creation must succeed");
    let target_body: serde_json::Value = serde_json::from_str(&target_raw).unwrap();
    let target_id = target_body["results"][0]["result"]["id"]
        .as_str()
        .expect("target id")
        .to_string();

    for (format, source_name) in [
        ("json", "AgentJsonChainSource"),
        ("auto", "AgentAutoChainSource"),
    ] {
        let chain_raw = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: format!(
                    r#"create(kind="entity", entity_kind="concept", name="{source_name}") | link(source_id=$prev.id, target_id="{target_id}", relation="extends")"#
                ),
                presentation: Some("agent".to_string()),
                presentation_per_op: None,
                save_to: None,
                format: Some(format.to_string()),
                format_per_op: None,
                request_id: None,
            })
            .await
            .unwrap_or_else(|error| panic!("agent/{format} chain request failed: {error}"));
        let chain_body: serde_json::Value = serde_json::from_str(&chain_raw).unwrap();
        assert_eq!(chain_body["summary"]["succeeded"], serde_json::json!(2));
        assert_eq!(chain_body["summary"]["failed"], serde_json::json!(0));
    }
}

/// (fmt-3) `presentation_per_op=verbose` pins a verbose op under
/// `format=auto` must preserve `full_id`, `namespace="local"`, and duplicate
/// `properties` keys — the redundancy-drop pre-pass must be skipped.
///
/// This is the regression pin for the bug fixed in this round: the batch
/// `presentation` was passed to `render_format` instead of the per-op
/// effective presentation, so `full_id`/`namespace`/duplicate-props could
/// be stripped even when that specific op was verbose.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn presentation_per_op_verbose_preserves_full_id_namespace_and_props() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Create a GTD task so we have a record with duplicated properties
    // (assignee/priority/status echoed in both top-level and `properties`).
    let create_params = RequestParams {
        plan: None,
        ops: r#"gtd.assign(title="verbose-pin-task", priority="p1", assignee="lambda:test")"#
            .to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let create_raw = server
        .dispatch_request_local(create_params)
        .await
        .expect("task creation must succeed");
    let create_body: serde_json::Value = serde_json::from_str(&create_raw).unwrap();
    let task_id = create_body["results"][0]["result"]["id"]
        .as_str()
        .expect("task id must be present");

    // Now fetch as a 2-op batch: op0 = agent (will be redundancy-dropped),
    // op1 = verbose (must survive the redundancy-drop pre-pass).
    //
    // We use gtd.tasks (which returns records with assignee/priority/status in both
    // top-level AND properties), then get the specific task in verbose mode.
    let batch_params = RequestParams {
        plan: None,
        ops: format!(r#"[gtd.tasks(limit=10), get(id="{task_id}")]"#),
        // Batch default: agent (will apply redundancy drop).
        presentation: Some("agent".to_string()),
        // Op1 overrides to verbose — must skip redundancy drop for that op.
        presentation_per_op: Some(vec![
            None,                        // op0 → inherits agent
            Some("verbose".to_string()), // op1 → verbose
        ]),
        save_to: None,
        format: Some("auto".to_string()),
        format_per_op: None,
        request_id: None,
    };

    let raw = server
        .dispatch_request_local(batch_params)
        .await
        .expect("batch dispatch must succeed");

    let body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON envelope");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "batch must return 2 results");

    // op0 was agent + auto → redundancy drop applied (namespace="local" elided).
    // The result is a rendered string; just verify it succeeded.
    assert_eq!(
        results[0]["ok"],
        serde_json::json!(true),
        "op0 (gtd.tasks) must succeed: {}",
        results[0]
    );

    // op1 was verbose + auto → redundancy drop MUST be skipped.
    assert_eq!(
        results[1]["ok"],
        serde_json::json!(true),
        "op1 (get task) must succeed: {}",
        results[1]
    );

    // The rendered result for op1 is a string (auto-formatted).
    // Under verbose mode the redundancy-drop pre-pass is skipped.
    //
    // Key differences vs. agent+auto:
    //   - In agent mode `present_response` shortens `id` to 8 chars and adds a
    //     separate `full_id` field; redundancy drop then removes that `full_id`.
    //   - In verbose mode `present_response` keeps the full 36-char UUID in `id`
    //     directly — there is NO separate `full_id` field to strip.
    //
    // What we can assert positively:
    //   - `namespace` appears (in agent+auto it is elided when "local" per §7.3).
    //   - `properties` keys that duplicate top-level fields (assignee, priority,
    //     status) survive (in agent+auto they are deduped away per §7.2).
    let op1_rendered = results[1]["result"]
        .as_str()
        .expect("op1 result must be a rendered string");

    // namespace: elided when "local" in auto+agent mode (§7.3); kept in verbose.
    assert!(
        op1_rendered.contains("namespace"),
        "verbose op: namespace must survive the redundancy-drop pre-pass; rendered: {op1_rendered}"
    );

    // properties: duplicate keys (assignee, priority, status) must survive in
    // verbose mode (§7.2 dedup is skipped).  In agent+auto these are removed.
    assert!(
        op1_rendered.contains("assignee"),
        "verbose op: properties.assignee must survive the redundancy-drop pre-pass; rendered: {op1_rendered}"
    );
    assert!(
        op1_rendered.contains("priority"),
        "verbose op: properties.priority must survive the redundancy-drop pre-pass; rendered: {op1_rendered}"
    );
}

/// (fmt-4) AlwaysVerbose verbs must skip the redundancy-drop pre-pass under
/// `format=auto` even with the DEFAULT Agent presentation and NO per-op override.
///
/// Pins the fix: `render_result` recomputed the format-time
/// presentation only from `presentation_per_op` → batch default, blind to the
/// `VerbPresentationPolicy::AlwaysVerbose` that `run_parsed` applies. So a
/// policy-verbose verb (`get`) under `format=auto` with the default Agent mode
/// was redundancy-dropped, stripping `namespace="local"` and duplicate
/// `properties` keys it is declared AlwaysVerbose to preserve. The fix folds the
/// AlwaysVerbose policy into the format-seam presentation. This is the *implicit
/// policy* sibling of `presentation_per_op_verbose_preserves_*` (explicit override).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn format_auto_always_verbose_verb_skips_redundancy_drop_without_override() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Create a GTD task: assignee/priority/status are echoed in both top-level
    // and `properties`, and the record carries namespace="local".
    let create_params = RequestParams {
        plan: None,
        ops: r#"gtd.assign(title="always-verbose-pin", priority="p1", assignee="lambda:test")"#
            .to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let create_raw = server
        .dispatch_request_local(create_params)
        .await
        .expect("task creation must succeed");
    let create_body: serde_json::Value = serde_json::from_str(&create_raw).unwrap();
    let task_id = create_body["results"][0]["result"]["id"]
        .as_str()
        .expect("task id must be present");

    // get() is AlwaysVerbose (ADR-045 §6). Dispatch it under format=auto with the
    // DEFAULT Agent presentation and NO presentation_per_op override. The
    // AlwaysVerbose policy must force Verbose at the format seam, so the
    // redundancy-drop pre-pass is skipped and namespace/properties survive.
    let get_params = RequestParams {
        plan: None,
        ops: format!(r#"get(id="{task_id}")"#),
        presentation: None,        // → default Agent
        presentation_per_op: None, // → no per-op override
        save_to: None,
        format: Some("auto".to_string()),
        format_per_op: None,
        request_id: None,
    };
    let raw = server
        .dispatch_request_local(get_params)
        .await
        .expect("get dispatch must succeed");

    let body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON envelope");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "single get must produce 1 result");
    assert_eq!(
        results[0]["ok"],
        serde_json::json!(true),
        "get must succeed: {}",
        results[0]
    );

    let rendered = results[0]["result"]
        .as_str()
        .expect("get result must be a rendered string under format=auto");

    // namespace="local" is elided under agent+auto (§7.3) but MUST survive for an
    // AlwaysVerbose verb — this is the regression the fix closes.
    assert!(
        rendered.contains("namespace"),
        "AlwaysVerbose get: namespace must survive redundancy-drop under format=auto + \
         default agent (no override); rendered: {rendered}"
    );
    // Duplicate properties keys (assignee/priority) are deduped under agent+auto
    // but MUST survive for an AlwaysVerbose verb.
    assert!(
        rendered.contains("assignee"),
        "AlwaysVerbose get: properties.assignee must survive redundancy-drop; rendered: {rendered}"
    );
    assert!(
        rendered.contains("priority"),
        "AlwaysVerbose get: properties.priority must survive redundancy-drop; rendered: {rendered}"
    );
}
