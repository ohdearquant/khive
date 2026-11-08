// ── clap / env-binding tests ───────────────────────────────────────────────

#[test]
#[serial]
fn khive_db_env_binds_to_db_arg() {
    if crate::test_process::run_in_child() {
        return;
    }

    // clap reads KHIVE_DB for `--db` (parity with `kkernel mcp`).
    std::env::set_var("KHIVE_DB", "/tmp/kkernel-exec-env.db");
    let args = ExecArgs::parse_from(["exec", "stats()"]);
    std::env::remove_var("KHIVE_DB");
    assert_eq!(args.db.as_deref(), Some("/tmp/kkernel-exec-env.db"));
}

#[test]
#[serial]
fn config_flag_and_env_bind_with_flag_precedence() {
    if crate::test_process::run_in_child() {
        return;
    }

    let previous = std::env::var_os("KHIVE_CONFIG");
    std::env::set_var("KHIVE_CONFIG", "/tmp/kkernel-exec-env-config.toml");

    let from_env = ExecArgs::parse_from(["exec", "stats()"]);
    assert_eq!(
        from_env.config.as_deref(),
        Some(std::path::Path::new("/tmp/kkernel-exec-env-config.toml"))
    );

    let from_flag = ExecArgs::parse_from([
        "exec",
        "stats()",
        "--config",
        "/tmp/kkernel-exec-flag-config.toml",
    ]);
    assert_eq!(
        from_flag.config.as_deref(),
        Some(std::path::Path::new("/tmp/kkernel-exec-flag-config.toml"))
    );

    match previous {
        Some(value) => std::env::set_var("KHIVE_CONFIG", value),
        None => std::env::remove_var("KHIVE_CONFIG"),
    }
}

#[test]
fn explicit_config_flag_parses_for_exec() {
    let args = ExecArgs::parse_from([
        "exec",
        "stats()",
        "--config",
        "/tmp/kkernel-exec-config.toml",
    ]);
    assert_eq!(
        args.config.as_deref(),
        Some(std::path::Path::new("/tmp/kkernel-exec-config.toml"))
    );
}

#[test]
fn actor_and_expect_actor_flags_parse_together() {
    let args = ExecArgs::parse_from([
        "exec",
        "stats()",
        "--actor",
        "lambda:worker",
        "--expect-actor",
        "lambda:worker",
    ]);
    assert_eq!(args.actor.as_deref(), Some("lambda:worker"));
    assert_eq!(args.expect_actor.as_deref(), Some("lambda:worker"));
}

#[test]
#[serial]
fn khive_actor_env_does_not_bind_to_explicit_actor_arg() {
    if crate::test_process::run_in_child() {
        return;
    }

    let previous = std::env::var("KHIVE_ACTOR").ok();
    std::env::set_var("KHIVE_ACTOR", "lambda:env");
    let args = ExecArgs::parse_from(["exec", "stats()"]);
    match previous {
        Some(value) => std::env::set_var("KHIVE_ACTOR", value),
        None => std::env::remove_var("KHIVE_ACTOR"),
    }
    assert_eq!(args.actor, None, "the env fallback must not become tier 1");
}

#[test]
fn actor_flags_conflict_with_pending_events_mode() {
    assert!(
        ExecArgs::try_parse_from(["exec", "--pending-events", "--actor", "lambda:worker",])
            .is_err()
    );
    assert!(ExecArgs::try_parse_from([
        "exec",
        "--pending-events",
        "--expect-actor",
        "lambda:worker",
    ])
    .is_err());
}

#[test]
fn explicit_actor_overrides_fallback_without_changing_namespace() {
    let mut cfg = RuntimeConfig {
        default_namespace: Namespace::parse("project:data").unwrap(),
        actor_id: Some("lambda:fallback".to_string()),
        visible_namespaces: vec![Namespace::parse("lambda:fallback").unwrap()],
        ..RuntimeConfig::default()
    };
    apply_actor_pin_and_expectation(&mut cfg, Some("lambda:cli"), Some("lambda:cli")).unwrap();
    assert_eq!(cfg.actor_id.as_deref(), Some("lambda:cli"));
    assert_eq!(cfg.default_namespace.as_str(), "project:data");
    assert_eq!(
        cfg.visible_namespaces,
        vec![Namespace::parse("lambda:cli").unwrap()],
        "pinning must drop the displaced actor's folded read visibility and add the pinned one"
    );
}

#[test]
fn explicit_local_actor_authoritatively_clears_fallback() {
    let mut cfg = RuntimeConfig {
        actor_id: Some("lambda:fallback".to_string()),
        visible_namespaces: vec![Namespace::parse("lambda:fallback").unwrap()],
        ..RuntimeConfig::default()
    };
    apply_actor_pin_and_expectation(&mut cfg, Some("local"), Some("local")).unwrap();
    assert_eq!(cfg.actor_id, None);
    assert!(
        cfg.visible_namespaces.is_empty(),
        "pinning to local must drop the displaced fallback actor's read visibility \
             without adding a replacement: {:?}",
        cfg.visible_namespaces
    );
}

#[test]
fn explicit_actor_pin_retains_unrelated_configured_visibility() {
    let mut cfg = RuntimeConfig {
        actor_id: Some("lambda:fallback".to_string()),
        visible_namespaces: vec![
            Namespace::parse("lambda:fallback").unwrap(),
            Namespace::parse("project:shared").unwrap(),
        ],
        ..RuntimeConfig::default()
    };
    apply_actor_pin_and_expectation(&mut cfg, Some("lambda:cli"), None).unwrap();
    assert_eq!(
        cfg.visible_namespaces,
        vec![
            Namespace::parse("project:shared").unwrap(),
            Namespace::parse("lambda:cli").unwrap(),
        ],
        "an explicitly configured extra visibility entry unrelated to the displaced \
             actor must survive the pin: {:?}",
        cfg.visible_namespaces
    );
}

#[test]
fn expect_actor_alone_validates_resolved_identity() {
    let mut cfg = RuntimeConfig {
        actor_id: Some("lambda:project".to_string()),
        ..RuntimeConfig::default()
    };
    apply_actor_pin_and_expectation(&mut cfg, None, Some("lambda:project")).unwrap();
    let err = apply_actor_pin_and_expectation(&mut cfg, None, Some("lambda:other"))
        .expect_err("a mismatched expectation must fail before dispatch");
    assert!(err.to_string().contains("--expect-actor mismatch"));
    assert!(err.to_string().contains("lambda:project"));
}

#[test]
fn actor_inputs_are_namespace_validated() {
    let mut cfg = RuntimeConfig::default();
    assert!(apply_actor_pin_and_expectation(&mut cfg, Some("bad actor"), None).is_err());
    assert!(apply_actor_pin_and_expectation(&mut cfg, None, Some("bad actor")).is_err());
}

#[tokio::test]
#[serial]
async fn authorized_explicit_actor_is_used_for_write_attribution() {
    if crate::test_process::run_in_child() {
        return;
    }

    let (previous_home, _home_dir) = isolate_home_for_test();
    let mut cfg = RuntimeConfig {
        db_path: None,
        actor_id: Some("lambda:fallback".to_string()),
        gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    apply_actor_pin_and_expectation(&mut cfg, Some("lambda:pinned"), None).unwrap();
    let server = build_local_fallback_server(cfg, &KhiveConfig::default(), None, None)
        .await
        .unwrap();
    let raw = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"comm.send(to="local", content="actor pin attribution")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .unwrap();
    restore_home(previous_home);

    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        response["results"][0]["ok"], true,
        "comm.send dispatch failed: {raw}"
    );
    assert_eq!(response["results"][0]["result"]["from"], "lambda:pinned");
}

#[test]
fn pending_events_flag_sets_mode() {
    let args = ExecArgs::parse_from(["exec", "--pending-events"]);
    assert!(args.pending_events);
    assert!(args.ops.is_none());
}

#[test]
fn pending_events_conflicts_with_ops() {
    let result = ExecArgs::try_parse_from(["exec", "--pending-events", "stats()"]);
    assert!(
        result.is_err(),
        "--pending-events and positional ops must conflict"
    );
}

#[test]
fn pending_events_conflicts_with_ops_file() {
    let result =
        ExecArgs::try_parse_from(["exec", "--pending-events", "--ops-file", "/tmp/x.jsonl"]);
    assert!(
        result.is_err(),
        "--pending-events and --ops-file must conflict"
    );
}

#[test]
fn ops_positional_is_optional() {
    // With --ops-file, the positional ops should be absent.
    let args = ExecArgs::parse_from(["exec", "--ops-file", "/tmp/batch.jsonl"]);
    assert!(args.ops.is_none());
    assert_eq!(
        args.ops_file.as_deref(),
        Some(std::path::Path::new("/tmp/batch.jsonl"))
    );
}

#[test]
fn ops_positional_works_without_pending_events() {
    let args = ExecArgs::parse_from(["exec", "stats()"]);
    assert_eq!(args.ops.as_deref(), Some("stats()"));
    assert!(!args.pending_events);
}

// ── ADR-045 §2: `kkernel exec` CLI surface defaults to Verbose ────────────

#[test]
fn presentation_defaults_to_verbose_when_flag_omitted() {
    // ADR-045 §2 selection rules: `kkernel exec` (a scripted/operator
    // surface) defaults to Verbose, unlike the MCP `request` tool (which
    // defaults to Agent at the envelope layer — see
    // `khive_mcp::server::parse_presentation_mode`, unchanged by this test).
    let args = ExecArgs::parse_from(["exec", "stats()"]);
    assert_eq!(args.presentation.as_deref(), Some("verbose"));
}

#[test]
fn presentation_agent_flag_still_selects_agent() {
    let args = ExecArgs::parse_from(["exec", "stats()", "--presentation", "agent"]);
    assert_eq!(args.presentation.as_deref(), Some("agent"));
}

#[test]
fn presentation_human_flag_still_selects_human() {
    let args = ExecArgs::parse_from(["exec", "stats()", "--presentation", "human"]);
    assert_eq!(args.presentation.as_deref(), Some("human"));
}

#[test]
fn dry_run_requires_ops_file() {
    // clap enforces `requires = "ops_file"` for --dry-run.
    let result = ExecArgs::try_parse_from(["exec", "stats()", "--dry-run"]);
    assert!(
        result.is_err(),
        "dry-run without --ops-file should be rejected by clap"
    );
}
