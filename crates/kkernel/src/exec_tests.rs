use super::*;
use clap::Parser;
use serial_test::serial;
use tempfile::NamedTempFile;
use uuid::Uuid;

// ── collect_op_failures: per-op error surfacing (#1228) ───────────────────

#[test]
fn collect_op_failures_extracts_reason_with_global_index() {
    let parsed = serde_json::json!({
        "results": [
            {"ok": true, "tool": "create", "result": {}},
            {"ok": false, "tool": "create", "error": "content rejected: suspected credential material"},
            {"ok": false, "tool": "link"},
        ],
        "summary": {"total": 3, "succeeded": 1, "failed": 2}
    });
    let failures = collect_op_failures(&parsed, 500, OpsFileReportMode::LegacyNoSave);
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["op_index"], 501);
    assert_eq!(failures[0]["tool"], "create");
    assert_eq!(
        failures[0]["error"],
        "content rejected: suspected credential material"
    );
    assert_eq!(failures[1]["op_index"], 502);
    assert_eq!(
        failures[1]["error"], "unknown error",
        "a failed entry with no error value still surfaces a placeholder"
    );
}

#[test]
fn collect_op_failures_preserves_structured_error_payloads() {
    let parsed = serde_json::json!({
        "results": [
            {"ok": false, "tool": "create",
             "error": {"kind": "invalid_input", "message": "content rejected"}},
        ],
        "summary": {"total": 1, "succeeded": 0, "failed": 1}
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    assert_eq!(
        failures[0]["error"],
        serde_json::json!({"kind": "invalid_input", "message": "content rejected"}),
        "structured KhiveError payloads pass through as JSON, not a placeholder"
    );
}

#[test]
fn collect_op_failures_preserves_stable_refusal_reason() {
    let parsed = serde_json::json!({
        "results": [
            {
                "ok": false,
                "tool": "not_loaded",
                "error": "unknown verb",
                "reason": "verb-refused"
            },
        ],
        "summary": {"total": 1, "succeeded": 0, "failed": 1}
    });
    let failures = collect_op_failures(&parsed, 9, OpsFileReportMode::BoundedSave);
    assert_eq!(failures[0]["reason"], "verb-refused");
    assert_eq!(failures[0]["op_index"], 9);
    let legacy = collect_op_failures(&parsed, 9, OpsFileReportMode::LegacyNoSave);
    assert!(
        legacy[0].get("reason").is_none(),
        "legacy no-save summary must retain its pre-reason wire shape"
    );
}

#[test]
fn collect_op_failures_empty_on_all_ok_or_missing_results() {
    let all_ok = serde_json::json!({
        "results": [{"ok": true, "tool": "stats", "result": {}}],
        "summary": {"total": 1, "succeeded": 1, "failed": 0}
    });
    assert!(collect_op_failures(&all_ok, 0, OpsFileReportMode::LegacyNoSave).is_empty());
    assert!(
        collect_op_failures(&serde_json::json!({}), 0, OpsFileReportMode::LegacyNoSave).is_empty()
    );
}

#[test]
fn no_save_reporting_matches_exact_legacy_golden() {
    let parsed = serde_json::json!({
        "results": [
            {"ok": true, "tool": "create", "result": {}},
            {"ok": false, "tool": "search", "error": "boom"},
        ],
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].get("aborted").is_none());
    let summary = ops_file_summary(OpsFileReportMode::LegacyNoSave, 2, 1, 1, 0, failures, 0);

    assert_eq!(
        ops_file_progress_line(OpsFileReportMode::LegacyNoSave, 2, 2, 1, 1, 0),
        "applied 2/2 (ok=1, failed=1)"
    );
    assert_eq!(
        serde_json::to_string_pretty(&summary).unwrap(),
        concat!(
            "{\n",
            "  \"failed\": 1,\n",
            "  \"failures\": [\n",
            "    {\n",
            "      \"error\": \"boom\",\n",
            "      \"op_index\": 1,\n",
            "      \"tool\": \"search\"\n",
            "    }\n",
            "  ],\n",
            "  \"succeeded\": 1,\n",
            "  \"total\": 2\n",
            "}"
        )
    );
    assert!(summary.get("aborted").is_none());
    assert!(summary.get("failure_details_omitted").is_none());
}

#[test]
fn no_save_reporting_retains_more_than_one_thousand_failures() {
    let parsed = serde_json::json!({
        "results": (0..=MAX_OPS_FILE_FAILURE_DETAILS)
            .map(|index| serde_json::json!({
                "ok": false,
                "tool": "create",
                "error": format!("failure-{index}"),
            }))
            .collect::<Vec<_>>(),
    });
    let mut retained = Vec::new();
    let mut omitted = 0;
    for failure in collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave) {
        assert!(retain_failure_detail(
            OpsFileReportMode::LegacyNoSave,
            failure,
            &mut retained,
            &mut omitted,
        ));
    }
    assert_eq!(retained.len(), MAX_OPS_FILE_FAILURE_DETAILS + 1);
    assert_eq!(omitted, 0);

    let summary = ops_file_summary(
        OpsFileReportMode::LegacyNoSave,
        retained.len(),
        0,
        retained.len(),
        0,
        retained,
        omitted,
    );
    assert_eq!(
        summary["failures"].as_array().unwrap().len(),
        MAX_OPS_FILE_FAILURE_DETAILS + 1
    );
    assert!(summary.get("failure_details_omitted").is_none());
}

#[test]
fn no_save_reporting_retains_error_larger_than_four_kib() {
    let large_error = "x".repeat(MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1);
    let parsed = serde_json::json!({
        "results": [{"ok": false, "tool": "create", "error": large_error}],
    });
    let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
    assert_eq!(
        failures[0]["error"].as_str().unwrap().len(),
        MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1
    );
    assert_eq!(failures[0]["error"], large_error);
}

#[test]
fn save_reporting_bounds_failure_count_and_error_detail() {
    let large_error = "x".repeat(MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1);
    let parsed = serde_json::json!({
        "results": (0..=MAX_OPS_FILE_FAILURE_DETAILS)
            .map(|index| serde_json::json!({
                "ok": false,
                "tool": "create",
                "aborted": index % 2 == 0,
                "error": if index == 0 {
                    serde_json::Value::String(large_error.clone())
                } else {
                    serde_json::Value::String(format!("failure-{index}"))
                },
            }))
            .collect::<Vec<_>>(),
    });
    let mut retained = Vec::new();
    let mut omitted = 0;
    for failure in collect_op_failures(&parsed, 0, OpsFileReportMode::BoundedSave) {
        retain_failure_detail(
            OpsFileReportMode::BoundedSave,
            failure,
            &mut retained,
            &mut omitted,
        );
    }
    assert_eq!(retained.len(), MAX_OPS_FILE_FAILURE_DETAILS);
    assert_eq!(omitted, 1);
    assert_eq!(retained[0]["aborted"], true);
    assert_eq!(
        retained[0]["error"],
        format!(
            "error detail omitted: exceeds {MAX_OPS_FILE_FAILURE_ERROR_BYTES}-byte ops-file diagnostic limit"
        )
    );

    let summary = ops_file_summary(
        OpsFileReportMode::BoundedSave,
        MAX_OPS_FILE_FAILURE_DETAILS + 1,
        0,
        MAX_OPS_FILE_FAILURE_DETAILS + 1,
        0,
        retained,
        omitted,
    );
    assert_eq!(summary["failure_details_omitted"], 1);
    assert_eq!(
        summary["failures"].as_array().unwrap().len(),
        MAX_OPS_FILE_FAILURE_DETAILS
    );
}

// ── HOME isolation for local-fallback tests ───────────────────────────────
//
// `build_local_fallback_server` (via `run_exec_inline_with_forward` /
// `run_exec_ops_file`) now loads `KhiveConfig::load_with_home_fallback`
// unconditionally, which falls through to `~/.khive/config.toml` (tier 4)
// when no project-local config is found. Any test that builds a
// `RuntimeConfig` directly (bypassing `resolve_runtime_config`) with
// `db_path: None` would otherwise pick up whatever REAL config a
// developer/CI machine happens to have at `$HOME/.khive/config.toml` —
// including a genuinely multi-backend one — and silently exercise the
// multi-backend arm (or open real backend files) instead of the isolated
// single-backend path the test assumes. Point `HOME` at an empty tempdir
// for the duration of any such test so `khive_cfg` resolves to
// `KhiveConfig::default()` deterministically, regardless of the host.
fn isolate_home_for_test() -> (Option<std::ffi::OsString>, tempfile::TempDir) {
    let prev = std::env::var_os("HOME");
    let dir = tempfile::tempdir().expect("tempdir for isolated HOME");
    std::env::set_var("HOME", dir.path());
    (prev, dir)
}

fn restore_home(prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}

const DAEMON_SPAWN_TEST_ENV_VARS: [&str; 8] = [
    "KHIVE_EMBEDDING_MODEL",
    "KHIVE_ADDITIONAL_EMBEDDING_MODELS",
    "KHIVE_ACTOR",
    "KHIVE_REQUIRE_ATTRIBUTED_ACTOR",
    "KHIVE_DB",
    "KHIVE_PACKS",
    "KHIVE_LOCK",
    "HOME",
];

struct EnvAndCwdGuard {
    original_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    original_cwd: std::path::PathBuf,
}

impl EnvAndCwdGuard {
    fn capture() -> Self {
        Self {
            original_env: DAEMON_SPAWN_TEST_ENV_VARS
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect(),
            original_cwd: std::env::current_dir().expect("read cwd"),
        }
    }
}

impl Drop for EnvAndCwdGuard {
    fn drop(&mut self) {
        for (name, original) in self.original_env.drain(..) {
            match original {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let _ = std::env::set_current_dir(&self.original_cwd);
    }
}

#[test]
#[serial]
fn daemon_spawn_env_guard_restores_every_mutated_variable() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _restore_machine_env = EnvAndCwdGuard::capture();
    for name in DAEMON_SPAWN_TEST_ENV_VARS {
        std::env::set_var(name, format!("sentinel-{name}"));
    }

    {
        let _guard = EnvAndCwdGuard::capture();
        for name in DAEMON_SPAWN_TEST_ENV_VARS {
            std::env::remove_var(name);
        }
    }

    for name in DAEMON_SPAWN_TEST_ENV_VARS {
        assert_eq!(
            std::env::var(name).as_deref(),
            Ok(format!("sentinel-{name}").as_str())
        );
    }
}

// ── acquire_local_construction_guard: in-memory dbs skip the guard ────────

#[test]
#[serial]
fn acquire_local_construction_guard_is_noop_for_in_memory_db() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

    let cfg = RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::default()
    };
    let guard = acquire_local_construction_guard(&cfg).expect("in-memory db needs no guard");
    assert!(
        guard.is_none(),
        "an in-memory database has no shared file to serialize construction against"
    );

    std::env::remove_var("KHIVE_LOCK");
}

// ── acquire_local_construction_guard: file-backed dbs serialize ──────────
//
// Two threads race to acquire the guard for the same file-backed db.
// Both must succeed (the guard is a blocking exclusive lock, not a
// try-and-fail check), but their guarded critical sections must never
// overlap — proven the same way
// `khive_runtime::daemon::tests::recovery_lock_serializes_two_concurrent_boot_sequences`
// proves it for the raw primitive: two threads increment/decrement a
// shared "inside the critical section" counter around a sleep, and a
// third-thread-visible max-observed-concurrency of 1 is the guarantee.

#[cfg(unix)]
#[test]
#[serial]
fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers() {
    if crate::test_process::run_in_child() {
        return;
    }

    acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl();
}

#[cfg(not(unix))]
#[test]
#[serial]
fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers_nonunix() {
    if crate::test_process::run_in_child() {
        return;
    }

    acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl();
}

fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let db_path = dir.path().join("cold.db3");

    let concurrent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let spawn_one = |label: &'static str| {
        let db_path = db_path.clone();
        let concurrent = concurrent.clone();
        let max_observed = max_observed.clone();
        std::thread::spawn(move || {
            let cfg = RuntimeConfig {
                db_path: Some(db_path),
                ..RuntimeConfig::default()
            };
            let guard = acquire_local_construction_guard(&cfg)
                .unwrap_or_else(|e| panic!("{label} must acquire the guard: {e}"));

            let now = concurrent.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_observed.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            concurrent.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

            drop(guard);
        })
    };

    let t_a = spawn_one("writer-a");
    let t_b = spawn_one("writer-b");
    t_a.join().expect("writer-a thread must not panic");
    t_b.join().expect("writer-b thread must not panic");

    assert_eq!(
        max_observed.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the two guarded critical sections must never overlap — the guard \
             failed to serialize concurrent local-construction callers"
    );

    std::env::remove_var("KHIVE_LOCK");
}

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

#[test]
fn serial_requires_ops_file_conflicts_with_inline_and_atomic_and_defaults_off() {
    let serial = ExecArgs::try_parse_from(["exec", "--ops-file", "/tmp/batch.jsonl", "--serial"])
        .expect("--serial must be accepted for a non-atomic ops-file");
    assert!(serial.serial);

    let default_parallel = ExecArgs::parse_from(["exec", "--ops-file", "/tmp/batch.jsonl"]);
    assert!(!default_parallel.serial);

    assert!(
        ExecArgs::try_parse_from(["exec", "stats()", "--serial"]).is_err(),
        "--serial without --ops-file must fail during CLI parsing"
    );
    assert!(
        ExecArgs::try_parse_from([
            "exec",
            "stats()",
            "--ops-file",
            "/tmp/batch.jsonl",
            "--serial",
        ])
        .is_err(),
        "--serial must not compose with inline positional ops"
    );
    assert!(
        ExecArgs::try_parse_from([
            "exec",
            "--ops-file",
            "/tmp/batch.jsonl",
            "--atomic",
            "--serial",
        ])
        .is_err(),
        "--serial and --atomic are distinct execution contracts and must conflict"
    );
}

#[test]
fn atomic_takes_no_inline_batch_of_chains() {
    // The cross-op atomic unit reads an ops file, one JSON op per line, so
    // no inline DSL reaches its admission check: a bracketed batch of
    // chains is refused at the same argument boundary as a flat batch.
    for ops in ["[stats() | stats(), stats()]", "[stats(), stats()]"] {
        let error = ExecArgs::try_parse_from(["exec", ops, "--atomic"])
            .expect_err("--atomic with inline ops must fail during CLI parsing");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "{ops}: {error}"
        );
    }
    let atomic = ExecArgs::try_parse_from(["exec", "--ops-file", "/tmp/batch.jsonl", "--atomic"])
        .expect("--atomic must be accepted with an ops file");
    assert!(atomic.atomic);
}

// ── isolated DB helpers ────────────────────────────────────────────────────

/// Build an isolated in-process runtime using a temp-file SQLite database.
/// Never touches the production `~/.khive/khive.db`.
fn isolated_server(db_path: &str) -> KhiveMcpServer {
    let cfg = RuntimeConfig {
        db_path: Some(PathBuf::from(db_path)),
        embedding_model: None,
        additional_embedding_models: vec![],
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (#1276) — callers of this helper
        // dispatch `kg` and `gtd.assign` verbs, so pin both rather than
        // letting a wider ambient pack set a developer's shell exports.
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime on temp db");
    KhiveMcpServer::new(rt).expect("server on temp db")
}

struct OpsFileConcurrencyProbePack {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reject_overlap: bool,
}

impl khive_types::Pack for OpsFileConcurrencyProbePack {
    const NAME: &'static str = "ops-file-concurrency-probe";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
        name: "reader_probe",
        description: "records test-only handler concurrency",
        visibility: khive_runtime::Visibility::Verb,
        category: khive_runtime::VerbCategory::Assertive,
        params: &[],
    }];
}

struct ProbeInFlightGuard {
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for ProbeInFlightGuard {
    fn drop(&mut self) {
        self.in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for OpsFileConcurrencyProbePack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        params: serde_json::Value,
        _registry: &khive_runtime::VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> std::result::Result<serde_json::Value, khive_runtime::RuntimeError> {
        let current = self
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let _guard = ProbeInFlightGuard {
            in_flight: self.in_flight.clone(),
        };
        self.max_in_flight
            .fetch_max(current, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        if params["fail"].as_bool().unwrap_or(false) {
            return Err(khive_runtime::RuntimeError::Internal(
                "requested probe failure".to_string(),
            ));
        }
        if self.reject_overlap && current > 1 {
            return Err(khive_runtime::RuntimeError::Internal(
                "sql_bridge.reader_open constrained-reader overlap".to_string(),
            ));
        }
        Ok(serde_json::json!({"sequence": params["sequence"]}))
    }
}

fn concurrency_probe_server(
    reject_overlap: bool,
) -> (
    KhiveMcpServer,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(OpsFileConcurrencyProbePack {
        in_flight: in_flight.clone(),
        max_in_flight: max_in_flight.clone(),
        reject_overlap,
    });
    (
        KhiveMcpServer::from_registry(builder.build().expect("probe registry")),
        in_flight,
        max_in_flight,
    )
}

fn reader_probe_ops(count: usize) -> Vec<OpsFileEntry> {
    (0..count)
        .map(|sequence| OpsFileEntry {
            tool: "reader_probe".to_string(),
            args: serde_json::json!({"sequence": sequence}),
        })
        .collect()
}

// ── isolated_server ignores ambient KHIVE_PACKS (#1276) ───────────────────
//
// `cargo test -p kkernel` failed ~20 exec tests whenever a developer's
// shell exported `KHIVE_PACKS` naming a pack not compiled into this
// workspace (e.g. `kg,gtd`): every `RuntimeConfig` built by this test
// module's shared helpers fell through to `RuntimeConfig::default()`'s
// `packs` field, which reads that env var, so construction panicked with
// `PackRegError { unknown: "gtd", .. }`. A unit test's outcome must not
// depend on ambient shell configuration.
#[test]
fn isolated_server_ignores_ambient_khive_packs_naming_unavailable_pack() {
    const CHILD_MARKER: &str = "KKERNEL_KHIVE_PACKS_TEST_CHILD";
    const TEST_NAME: &str =
        "exec::tests::isolated_server_ignores_ambient_khive_packs_naming_unavailable_pack";

    if std::env::var_os(CHILD_MARKER).is_none() {
        let status =
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .arg(TEST_NAME)
                .arg("--exact")
                .env("KHIVE_PACKS", "kg,gtd")
                .env(CHILD_MARKER, "1")
                .status()
                .expect("spawn isolated KHIVE_PACKS test process");
        assert!(status.success(), "isolated child test failed: {status}");
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    // Before the fix, this panicked inside `KhiveMcpServer::new` — the
    // helper inherited the ambient list above instead of pinning its own.
    let _server = isolated_server(&db_path);
}

fn rerun_in_command_scoped_empty_home(child_marker: &str, test_name: &str) -> bool {
    if std::env::var_os(child_marker).is_some() {
        return false;
    }

    let calling_process_home = std::env::var_os("HOME");
    let empty_home = tempfile::tempdir().expect("isolated child HOME");
    let status =
        std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .arg(test_name)
            .arg("--exact")
            .env("HOME", empty_home.path())
            .env_remove("KHIVE_EMBEDDING_MODEL")
            .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
            .env_remove("KHIVE_ACTOR")
            .env(child_marker, "1")
            .status()
            .expect("spawn isolated config-discovery test process");
    assert_eq!(
        std::env::var_os("HOME"),
        calling_process_home,
        "command-scoped HOME must not mutate the calling test process"
    );
    assert!(status.success(), "isolated child test failed: {status}");
    true
}

// ── exec-path / serve-path config_id parity (#581) ────────────────────────
//
// `run_exec`'s cfg construction (above) and `kkernel mcp`'s `build_server`
// both call `resolve_runtime_config`. These tests prove the two call shapes
// agree on `compute_config_id` for the same database — the acceptance gate
// for the #581 fix — and settle the `namespace_explicit` design question
// empirically rather than by convention.

/// Direct regression guard for #581: a project's tier-3 `.khive/config.toml`
/// `[actor] id` must be visible to `kkernel exec` exactly as it is to
/// `kkernel mcp`, and the two paths' `config_id` must be byte-identical so
/// the daemon accepts exec's forwarded frame instead of rejecting it as a
/// `ConfigMismatch` (which silently falls back to an anonymous in-process
/// dispatch — the reported symptom: `comm.inbox` returning `count=0`).
#[test]
#[serial]
fn exec_config_id_matches_serve_config_id_for_project_toml_actor() {
    const CHILD_MARKER: &str = "KKERNEL_EXEC_PROJECT_CONFIG_TEST_CHILD";
    const TEST_NAME: &str =
        "exec::tests::exec_config_id_matches_serve_config_id_for_project_toml_actor";
    if rerun_in_command_scoped_empty_home(CHILD_MARKER, TEST_NAME) {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let khive_dir = dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    std::fs::write(
        khive_dir.join("config.toml"),
        r#"
[actor]
id = "lambda:test-actor"

[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
    )
    .expect("write config.toml");

    // A db path anchored INSIDE the same `.khive` dir — this is what makes
    // tier-3 discovery agree between a client and a daemon serving the same
    // database, regardless of process cwd (see `project_config_anchor_dir`).
    let db_path = khive_dir.join("exec-parity-test.db");
    let db_str = db_path.to_str().expect("utf8 path").to_string();

    let ns = Namespace::parse("local").expect("ns");

    // Exec-shaped inputs with no explicit config in this scenario and
    // `namespace_explicit: true` (the choice made in `run_exec` above).
    // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
    // from the ambient environment (same rationale as `isolated_server`
    // above, #1276): `RuntimeConfig::default()` reads `KHIVE_PACKS` fresh
    // on every call, so leaving this `None` makes the assertion below
    // depend on two independent env reads observing the same ambient
    // value — a real flake source when a concurrently-running test
    // mutates process env between them (#1356).
    let pinned_packs = Some(vec!["kg".to_string()]);

    let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&db_str),
        config: None,
        namespace: ns.clone(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs.clone(),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    // serve-shaped inputs: mirrors `build_server` when the operator starts
    // `kkernel mcp --daemon` with no explicit --actor/--namespace flag,
    // relying on the config file's `[actor] id` — the common daemon-startup
    // shape (`resolve_cli_namespace` returns `explicit=false` in that case).
    let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&db_str),
        config: None,
        namespace: ns,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs,
        brain_profile: None,
    })
    .expect("resolve serve-shaped config");

    // The TOML must actually have reached both constructions — the direct
    // regression proxy for #581, verified without a live daemon socket.
    assert_eq!(exec_cfg.actor_id.as_deref(), Some("lambda:test-actor"));
    assert_eq!(serve_cfg.actor_id.as_deref(), Some("lambda:test-actor"));
    assert!(
        exec_cfg
            .visible_namespaces
            .contains(&Namespace::parse("lambda:test-actor").expect("ns")),
        "actor.id must fold into visible_namespaces (ADR-007 Rev 4 Rule 3b)"
    );
    assert!(
        exec_cfg.embedding_model.is_some(),
        "config-file [[engines]] must resolve an embedding model, not env/default"
    );
    assert_eq!(
        format!("{:?}", exec_cfg.embedding_model),
        format!("{:?}", serve_cfg.embedding_model),
    );

    // The acceptance gate: byte-identical config_id, so the daemon accepts
    // exec's forwarded frame instead of rejecting it as a ConfigMismatch.
    assert_eq!(
        compute_config_id(&exec_cfg, None),
        compute_config_id(&serve_cfg, None),
        "exec-path config_id must match the serve/daemon-path config_id for the same db"
    );
}

/// Regression guard: an explicit `--actor` pin must rebuild the
/// actor-derived portion of `visible_namespaces`, not just `actor_id`.
///
/// Builds the config exactly the way `run_exec` does — through
/// `resolve_runtime_config`, from a project `[actor] id = "lambda:fallback"`
/// with no explicit extra visibility — so the displaced actor is folded
/// into `visible_namespaces` (ADR-007 Rev 4 Rule 3b) before the pin is
/// ever applied. A non-local pin must give default reads `local ∪
/// lambda:pinned`; a `local` pin must leave only `local`. Neither case may
/// retain `lambda:fallback`.
#[test]
#[serial]
fn actor_pin_rebuilds_visible_namespaces_dropping_displaced_fallback() {
    const CHILD_MARKER: &str = "KKERNEL_ACTOR_PIN_CONFIG_TEST_CHILD";
    const TEST_NAME: &str =
        "exec::tests::actor_pin_rebuilds_visible_namespaces_dropping_displaced_fallback";
    if rerun_in_command_scoped_empty_home(CHILD_MARKER, TEST_NAME) {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let khive_dir = dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    std::fs::write(
        khive_dir.join("config.toml"),
        r#"
[actor]
id = "lambda:fallback"
"#,
    )
    .expect("write config.toml");

    let db_path = khive_dir.join("actor-pin-visibility-test.db");
    let db_str = db_path.to_str().expect("utf8 path").to_string();
    let pinned_packs = Some(vec!["kg".to_string()]);

    let resolve = |db: &str| {
        resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: pinned_packs.clone(),
            brain_profile: None,
        })
        .expect("resolve exec-shaped config")
    };

    // Sanity: the fallback actor really is folded into the default read
    // visible-set before any pin is applied — otherwise this test would
    // pass vacuously.
    let baseline = resolve(&db_str);
    assert_eq!(baseline.actor_id.as_deref(), Some("lambda:fallback"));
    assert!(baseline
        .visible_namespaces
        .contains(&Namespace::parse("lambda:fallback").expect("ns")));

    // A non-local pin must replace the fallback's read visibility with the
    // pinned actor's — never both, never neither.
    let mut pinned_cfg = resolve(&db_str);
    apply_actor_pin_and_expectation(&mut pinned_cfg, Some("lambda:pinned"), None).unwrap();
    assert_eq!(pinned_cfg.actor_id.as_deref(), Some("lambda:pinned"));
    assert!(
        pinned_cfg
            .visible_namespaces
            .contains(&Namespace::parse("lambda:pinned").expect("ns")),
        "pinned actor must be added to the default read scope: {:?}",
        pinned_cfg.visible_namespaces
    );
    assert!(
        !pinned_cfg
            .visible_namespaces
            .contains(&Namespace::parse("lambda:fallback").expect("ns")),
        "the displaced fallback actor must not remain visible under the pinned \
             identity: {:?}",
        pinned_cfg.visible_namespaces
    );

    // A `local` pin must authoritatively clear the fallback's visibility
    // without adding a replacement.
    let mut local_cfg = resolve(&db_str);
    apply_actor_pin_and_expectation(&mut local_cfg, Some("local"), None).unwrap();
    assert_eq!(local_cfg.actor_id, None);
    assert!(
        local_cfg.visible_namespaces.is_empty(),
        "pinning to local must leave only local visible, retaining neither the \
             fallback actor nor adding a new one: {:?}",
        local_cfg.visible_namespaces
    );
}

/// Settles the `namespace_explicit` design question by constructing both
/// arms and comparing `compute_config_id` directly, per the decision
/// criterion: does either arm break config_id parity with the daemon?
///
/// No `[actor] id` is present (an explicit EMPTY config file makes
/// this fully deterministic — no dependency on cwd or `$HOME`), and the
/// namespace is a non-"local" value so the actor_id fill-when-None guard in
/// `resolve_runtime_config` (the ONLY place `namespace_explicit` has any
/// effect in the embed path, i.e. `no_embed: false`, which `kkernel exec`
/// always uses) actually fires for one arm and not the other.
#[test]
#[serial]
fn namespace_explicit_changes_actor_id_fill_but_not_config_id() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");

    // A real, EMPTY config file: the explicit tier fails loud on a
    // missing file (ADR-035), so the hermeticity trick must be a real
    // file with no `[actor]` block.
    let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
    let missing_config = empty_config_dir.path().join("config.toml");
    std::fs::write(&missing_config, "").expect("write empty config");
    let ns = Namespace::parse("lambda:custom-ns").expect("ns");
    // Pin packs so the `compute_config_id` comparison below never depends
    // on two independent `KHIVE_PACKS` env reads agreeing (#1356).
    let pinned_packs = Some(vec!["kg".to_string()]);

    let with_explicit_true = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns.clone(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs.clone(),
        brain_profile: None,
    })
    .expect("resolve with namespace_explicit=true");

    let with_explicit_false = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs,
        brain_profile: None,
    })
    .expect("resolve with namespace_explicit=false");

    // The fill-when-None guard DOES fire differently between the two arms...
    assert_eq!(
        with_explicit_true.actor_id.as_deref(),
        Some("lambda:custom-ns"),
        "namespace_explicit=true + non-local namespace + no config actor.id \
             must fill actor_id from the namespace (ADR-057)"
    );
    assert_eq!(
        with_explicit_false.actor_id, None,
        "namespace_explicit=false must NOT fill actor_id"
    );

    // ...but `compute_config_id` never reads identity fields (`actor_id` or
    // `visible_namespaces`; namespace is carried separately per its own doc
    // comment), so the two configs — which differ ONLY in actor_id — must
    // still produce a byte-identical fingerprint. This is the empirical
    // basis for `run_exec` picking `namespace_explicit: true`: it is the
    // conservative, behavior-preserving choice, and it provably does not
    // affect config_id parity with the daemon either way.
    assert_eq!(
        compute_config_id(&with_explicit_true, None),
        compute_config_id(&with_explicit_false, None),
        "namespace_explicit must not affect the daemon-forwarded config_id"
    );
}

/// D1-R3: the two tests above are inert to the config_id topology-drift
/// bug because they always call `compute_config_id(_, None)` on BOTH
/// sides — omitting the backends topology can never diverge from itself.
/// This test constructs a genuinely multi-backend `KhiveConfig` (mirroring
/// the real hosted shape: a `main` backend plus a separate `sessions`
/// backend, with the `session` pack pinned to it) and proves both that the
/// pre-fix computation diverges and that the post-fix computation is
/// byte-identical.
#[test]
#[serial]
fn exec_config_id_matches_serve_config_id_for_multi_backend_topology() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_runtime::{BackendConfig, BackendKind, PackConfig};

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");

    // An explicit EMPTY config file keeps this fully deterministic
    // regardless of host state (same rationale as the sibling test
    // above; the explicit tier fails loud on a MISSING file — ADR-035 —
    // so the trick must be a real file).
    let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
    let missing_config = empty_config_dir.path().join("multi-backend-config.toml");
    std::fs::write(&missing_config, "").expect("write empty config");
    let ns = Namespace::parse("local").expect("ns");

    let khive_cfg = KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(std::path::PathBuf::from("/tmp/khive-parity-main.db")),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
            BackendConfig {
                name: "sessions".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(std::path::PathBuf::from("/tmp/khive-parity-sessions.db")),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
        ],
        packs: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "session".to_string(),
                PackConfig {
                    backend: "sessions".to_string(),
                    no_embed: false,
                },
            );
            m
        },
        ..KhiveConfig::default()
    };

    // Pin packs so the config_id comparisons below never depend on two
    // independent `KHIVE_PACKS` env reads agreeing (#1356).
    let pinned_packs = Some(vec!["kg".to_string()]);

    // exec-shaped inputs (namespace_explicit: true — the choice `run_exec` makes).
    let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns.clone(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs.clone(),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    // serve-shaped inputs (namespace_explicit: false — the daemon-startup shape).
    let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs,
        brain_profile: None,
    })
    .expect("resolve serve-shaped config");

    // Pre-fix proof: the OLD exec-path computation (`compute_config_id(_, None)`,
    // exec.rs:490 before this fix) diverges from the daemon/serve-path
    // computation (`Some(&khive_cfg)`, serve.rs:916) the instant the backends
    // topology is non-empty. This is the exact bug: a legitimately-matching
    // client was rejected as a `ConfigMismatch` and silently fell back to the
    // cold in-process path on every call.
    assert_ne!(
        compute_config_id(&exec_cfg, None),
        compute_config_id(&serve_cfg, Some(&khive_cfg)),
        "pre-fix exec computation (None) must diverge from the daemon computation \
             (Some) for a non-empty backends topology — proves this test catches the \
             real divergence, not a tautology"
    );

    // Post-fix proof: both sides fold the SAME backends topology and produce
    // a byte-identical fingerprint, so the daemon accepts the forwarded frame
    // instead of rejecting it as a ConfigMismatch.
    assert_eq!(
        compute_config_id(&exec_cfg, Some(&khive_cfg)),
        compute_config_id(&serve_cfg, Some(&khive_cfg)),
        "exec-path config_id must match the daemon-path config_id for the same \
             multi-backend topology (D1 fix acceptance gate)"
    );
}

// ── build_local_fallback_server multi-backend routing (D1-R2) ────────────
//
// Before this fix, both of exec's local-dispatch call sites always built a
// single-backend runtime pointed at `cfg.db_path`, regardless of any
// `[[backends]]` declaration in `khive_cfg`. A config pinning a pack (e.g.
// `comm`) to a separate backend would have that pack's writes silently
// land in whatever single file `cfg.db_path` pointed at instead of the
// declared backend file. This test pins `comm` to a second, file-backed
// `secondary` backend and proves the write lands there — not in `main` —
// by re-opening each backend file independently afterward.

/// D1-R2 regression proof: `build_local_fallback_server` must delegate to
/// `build_server_multi_backend` (not the single-backend `KhiveMcpServer::new`)
/// whenever `khive_cfg.backends` is non-empty, and pack routing must actually
/// take effect end to end.
#[tokio::test]
#[serial]
async fn build_local_fallback_server_routes_through_multi_backend_when_backends_declared() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_runtime::{BackendConfig, BackendKind, PackConfig};

    let lock_dir = tempfile::tempdir().expect("private construction lock");
    let _env = EnvAndCwdGuard::capture();
    std::env::set_var("KHIVE_LOCK", lock_dir.path().join("khived.recovery.lock"));

    let main_db = NamedTempFile::new().expect("main db tempfile");
    let secondary_db = NamedTempFile::new().expect("secondary db tempfile");
    let main_path = main_db.path().to_path_buf();
    let secondary_path = secondary_db.path().to_path_buf();

    let khive_cfg = KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
            BackendConfig {
                name: "secondary".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(secondary_path.clone()),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
        ],
        packs: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "comm".to_string(),
                PackConfig {
                    backend: "secondary".to_string(),
                    no_embed: false,
                },
            );
            m
        },
        ..KhiveConfig::default()
    };

    // `db_path` here is NOT the actual storage location when `[[backends]]`
    // is declared — `build_server_multi_backend` opens each backend's own
    // declared path (the tempfiles above) independently. It is only the
    // identity/fingerprint value `assert_captured_db_anchor_consistent` checks
    // against `resolve_db_anchor(cli_db_override)`, exactly mirroring what
    // a real `kkernel exec` invocation with NO explicit `--db` flag would
    // resolve to (the realistic shape when `[[backends]]` fully governs
    // storage) — see `base_runtime_config_for_multi_backend` in serve.rs's
    // own multi-backend test suite for the identical pattern.
    let cfg = RuntimeConfig {
        db_path: khive_runtime::resolve_db_anchor(None),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: Some("actor-routing-test".to_string()),
        ..RuntimeConfig::default()
    };

    // No explicit `--db` override — `[[backends]]` alone governs storage,
    // matching the `cfg.db_path` shape above. An explicit override here
    // would be rejected as ambiguous by `build_registry_for_multi_backend`
    // (ADR-028 §8) since 2 backends are already declared.
    let db_anchor = cfg.db_path.clone();
    let server = build_local_fallback_server(cfg, &khive_cfg, None, db_anchor.as_deref())
        .await
        .expect("multi-backend local fallback must build");
    assert!(lock_dir.path().join("khived.recovery.lock").is_file());

    let send = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"comm.send(to="actor-routing-test", content="routed-via-secondary", self_send=true)"#
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("comm.send must dispatch");
    let send_resp: serde_json::Value = serde_json::from_str(&send).expect("valid JSON");
    assert_eq!(
        send_resp["results"][0]["ok"].as_bool(),
        Some(true),
        "comm.send must succeed through the multi-backend fallback server: {send_resp}"
    );

    // Inspect each physical file directly: list routing and mailbox
    // filtering must not influence the backend-placement assertion.
    fn count_messages(db_path: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read-only connection to backend file");
        conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE kind = 'message' AND content = ?1",
            ["routed-via-secondary"],
            |row| row.get(0),
        )
        .expect("count persisted message notes")
    }

    let main_count = count_messages(&main_path);
    let secondary_count = count_messages(&secondary_path);

    assert_eq!(
        main_count, 0,
        "comm pack must NOT write into the `main` backend file when pinned to \
             `secondary` (D1-R2: a silent single-backend fallback would have written \
             it here instead)"
    );
    assert_eq!(
        secondary_count, 2,
        "comm pack write must land in its declared `secondary` backend file — \
             `comm.send` dual-writes an outbound + inbound note copy per message \
             (khive-pack-comm's message.rs), both via the SAME pack runtime, so a \
             single self-send yields 2 `message` notes in whichever backend `comm` \
             is pinned to"
    );
}

#[tokio::test]
#[serial]
async fn build_local_fallback_server_uses_captured_anchor_after_home_changes() {
    if crate::test_process::run_in_child() {
        return;
    }

    let (previous_home, _first_home) = isolate_home_for_test();
    let cfg = RuntimeConfig {
        db_path: khive_runtime::resolve_db_anchor(None),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let db_anchor = cfg.db_path.clone();
    let khive_cfg = KhiveConfig {
        backends: vec![khive_runtime::BackendConfig {
            name: "main".to_string(),
            kind: khive_runtime::BackendKind::Memory,
            path: None,
            cache_mb: None,
            journal_mode: None,
            served_kinds: None,
            read_only: false,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
        }],
        ..KhiveConfig::default()
    };
    let second_home = tempfile::tempdir().expect("second HOME");
    std::env::set_var("HOME", second_home.path());

    let result = build_local_fallback_server(cfg, &khive_cfg, None, db_anchor.as_deref()).await;
    restore_home(previous_home);

    assert!(
        result.is_ok(),
        "exec fallback must use the anchor captured with RuntimeConfig after HOME changes: {}",
        result.err().unwrap()
    );
}

// ── single-backend fallback installs a BlobStore (khive#1209) ────────────
//
// Before this fix, `build_local_fallback_server`'s single-backend branch
// constructed `KhiveRuntime`/`KhiveMcpServer` without ever calling
// `install_resolved_blob_store`, so `blob.*` verbs dispatched through
// `kkernel exec`'s in-process fallback always saw an unconfigured
// `BlobStore` even when the same config/backend combination resolves one
// for the `serve` daemon boot path. `KhiveMcpServer` does not expose its
// wrapped runtime, so this asserts the same *observable* side effect the
// `serve` path's own tests rely on: `FsBlobStore::new` (khive-db
// `stores/blob.rs`) creates its root directory eagerly. With no
// `[storage.blob]` config and no `KHIVE_BLOB_ROOT`, resolution falls
// back to `<db_dir>/blobs` — that directory existing after construction
// is proof the install call ran.
#[tokio::test]
#[serial]
async fn build_local_fallback_server_installs_blob_store_single_backend() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvAndCwdGuard::capture();
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let db_path = dir.path().join("exec_blob.db");
    let cfg = RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: RuntimeConfig::built_in_packs(),
        ..RuntimeConfig::default()
    };
    let khive_cfg = KhiveConfig::default();

    let _server = build_local_fallback_server(cfg, &khive_cfg, None, None)
        .await
        .expect("single-backend local-exec construction must succeed");
    assert!(dir.path().join("khived.recovery.lock").is_file());

    assert!(
        dir.path().join("blobs").is_dir(),
        "default <db_dir>/blobs root must exist after construction, proving \
             install_resolved_blob_store ran for the single-backend fallback path"
    );
}

// ── guarded local construction races a guarded boot (#667/#645) ──────────
//
// Mirrors `khive-runtime/tests/cold_boot_fts_race.rs`'s deterministic
// two-thread pattern, but races a `kkernel mcp --daemon`-style guarded
// boot against `build_local_fallback_server` itself — the exact local
// path that, before this fix, constructed `KhiveRuntime`/`KhiveMcpServer`
// without acquiring the boot guard at all. Both "boots" target the SAME
// fresh (cold) db file; if either side ran unguarded, migrations/FTS DDL
// could interleave and corrupt (or lose rows from) the `fts_notes` index.

#[cfg(unix)]
fn run_one_guarded_daemon_boot(
    db_path: std::path::PathBuf,
    writer_label: &'static str,
    count: usize,
) {
    let guard = khive_runtime::daemon::acquire_recovery_lock().expect("acquire daemon boot guard");

    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build per-thread tokio runtime");

    rt_handle.block_on(async {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("cold-boot migrations succeed");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..count {
            rt.create_note(
                &token,
                "memo",
                None,
                &format!("{writer_label} note {i} — boot race marker"),
                None,
                None,
                vec![],
            )
            .await
            .expect("note write must succeed inside the guarded boot window");
        }
    });

    drop(guard);
}

#[cfg(unix)]
fn run_one_local_exec_construction(
    db_path: std::path::PathBuf,
    writer_label: &'static str,
    count: usize,
) {
    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build per-thread tokio runtime");

    rt_handle.block_on(async {
        let cfg = RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            // Pin the pack list explicitly rather than inheriting
            // `KHIVE_PACKS` from the ambient environment (#1276) — this
            // race only exercises `kg` writes.
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let khive_cfg = KhiveConfig::default();
        // The exact call site under test: before this fix, this function
        // built `KhiveRuntime`/`KhiveMcpServer` without acquiring any
        // guard, so it could run migrations/FTS DDL concurrently with
        // the guarded boot above against the same file.
        let server = build_local_fallback_server(cfg, &khive_cfg, None, None)
            .await
            .expect("guarded local-exec construction must succeed");

        for i in 0..count {
            let params = RequestParams {
                plan: None,
                ops: format!(
                    r#"create(kind="observation", content="{writer_label} note {i} — boot race marker")"#
                ),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            };
            let raw = server
                .dispatch_request_local(params)
                .await
                .expect("dispatch must succeed inside the guarded construction window");
            let resp: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
            assert_eq!(
                resp["results"][0]["ok"],
                serde_json::json!(true),
                "write must succeed: {resp}"
            );
        }
    });
}

// ── deterministic lock-blocking oracle ────────────────────────────────────
//
// The end-to-end race test below proves no corruption results when both
// sides respect the guard, but a mutation-testing pass showed its
// final-row-count oracle does NOT fail if the guard at
// `build_local_fallback_server`'s call site is removed entirely: with no
// second real lock-holder racing it, the row count comes out right either
// way, so the test cannot tell "guarded" from "unguarded". This test
// closes that gap: it holds the SAME recovery lock the guard acquires
// from the test thread itself, then asserts `build_local_fallback_server`
// cannot complete construction while that lock is held (bounded wait) —
// an assertion that is trivially true when the guard is unguarded (it
// never acquires anything, so it isn't blocked by our held lock).
#[cfg(unix)]
#[test]
#[serial]
fn build_local_fallback_server_blocks_while_recovery_lock_is_held() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    let db_path = dir.path().join("guard_block_test.db3");

    // A separate file descriptor to the SAME lock path — flock's
    // blocking semantics apply per open-file-description, so this
    // blocks a second acquirer even from another thread in this same
    // process (the same pattern `daemon.rs`'s own
    // `recovery_lock_serializes_two_concurrent_boot_sequences` and
    // `cold_boot_fts_race.rs` rely on).
    let held_guard =
        khive_runtime::daemon::acquire_recovery_lock().expect("acquire recovery lock in test");

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt_handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build per-thread tokio runtime");
        let cfg = RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            // Pin the pack list explicitly rather than inheriting
            // `KHIVE_PACKS` from the ambient environment (#1276).
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let khive_cfg = KhiveConfig::default();
        // The exact call under test: every non-atomic local-exec path
        // (daemon-unreachable fallback, --save-file, KHIVE_NO_DAEMON=1,
        // non-atomic --ops-file) funnels through this one function.
        let result = rt_handle
            .block_on(async { build_local_fallback_server(cfg, &khive_cfg, None, None).await });
        // Sent only AFTER construction returns — the test observes
        // whether this arrives before or after the lock is released.
        let _ = tx.send(());
        result
    });

    // Bounded wait: construction must NOT complete while the lock is
    // held. If the production guard at `build_local_fallback_server`'s
    // call site is ever removed or no-op'd, nothing blocks this thread
    // and the signal arrives well inside this window — this is the
    // mutation-killing assertion.
    let completed_while_locked = rx
        .recv_timeout(std::time::Duration::from_millis(500))
        .is_ok();
    assert!(
        !completed_while_locked,
        "build_local_fallback_server must NOT complete while the boot/recovery \
             lock is held by another holder — if this fires, the guard at its \
             production call site has been removed or stopped acquiring the shared lock"
    );

    drop(held_guard);

    handle
        .join()
        .expect("construction thread must not panic")
        .expect("construction must succeed once the lock is released");

    std::env::remove_var("KHIVE_LOCK");
}

// Named serial key (not the bare `#[serial]` default): this test only
// touches `KHIVE_LOCK`, not the `KHIVE_REQUIRE_ATTRIBUTED_ACTOR` /
// `KHIVE_NO_DAEMON` / `HOME` vars the default-keyed `#[serial]` tests
// above guard. Sharing their queue would only add wall-clock delay
// (this test spawns two real OS threads doing real `flock` + migrations)
// without protecting anything — and empirically DOES perturb unrelated
// non-serial tests elsewhere in this binary (`pending_events`) that race
// on those other env vars.
#[cfg(unix)]
#[test]
#[serial]
fn local_exec_construction_races_guarded_daemon_boot_without_fts_corruption() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    // Fresh (cold) database file — neither side has run migrations on it yet.
    let db_path = dir.path().join("local_exec_boot_race.db3");

    const PER_WRITER: usize = 10;
    let path_a = db_path.clone();
    let path_b = db_path.clone();

    let t_a =
        std::thread::spawn(move || run_one_guarded_daemon_boot(path_a, "daemon-boot", PER_WRITER));
    let t_b = std::thread::spawn(move || {
        run_one_local_exec_construction(path_b, "local-exec", PER_WRITER)
    });
    t_a.join().expect("daemon-boot thread must not panic");
    t_b.join().expect("local-exec thread must not panic");

    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build verification tokio runtime");
    rt_handle.block_on(async {
        let verify_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path.clone()),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("post-race runtime opens cleanly");
        let token = verify_rt
            .authorize(Namespace::local())
            .expect("authorize local");

        let hits = verify_rt
            .search_notes(
                &token,
                "boot race marker",
                None,
                100,
                None,
                false,
                &[],
                None,
            )
            .await
            .expect("FTS search over notes must succeed, not error on a corrupted index");
        assert_eq!(
            hits.len(),
            PER_WRITER * 2,
            "every planted note from both writers must be present and \
                 FTS-searchable — a corrupted/partial index would drop or \
                 duplicate rows: {hits:?}"
        );
    });

    std::env::remove_var("KHIVE_LOCK");
}

// ── parse_ops_file tests ───────────────────────────────────────────────────

#[test]
fn parse_ops_file_skips_blank_lines() {
    use std::io::Write as _;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
    f.write_all(b"\n").unwrap(); // blank
    f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
    let ops = parse_ops_file(f.path()).unwrap();
    assert_eq!(ops.len(), 2);
}

#[test]
fn parse_ops_file_reports_line_number_on_malformed() {
    use std::io::Write as _;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
    f.write_all(b"not-json\n").unwrap(); // line 2 is bad
    let err = parse_ops_file(f.path()).unwrap_err();
    assert_eq!(
        err.downcast_ref::<ExecRefusal>().map(|error| error.reason),
        Some(RefusalReason::ParseError)
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("line 2"),
        "error should name the bad line number, got: {msg}"
    );
}

#[test]
fn parse_ops_file_missing_tool_field() {
    use std::io::Write as _;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{\"notool\":\"x\",\"args\":{}}\n").unwrap();
    let err = parse_ops_file(f.path()).unwrap_err();
    assert_eq!(
        err.downcast_ref::<ExecRefusal>().map(|error| error.reason),
        Some(RefusalReason::ParseError)
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("line 1"), "should report line number: {msg}");
}

#[test]
fn atomic_op_limit_is_checked_before_snapshot_materialization() {
    struct PanicOnSnapshotAccess;

    impl std::io::Read for PanicOnSnapshotAccess {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("over-limit atomic snapshot must not be read or materialized")
        }
    }

    impl std::io::Seek for PanicOnSnapshotAccess {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            panic!("over-limit atomic snapshot must not be rewound or materialized")
        }
    }

    let error = parse_atomic_validated_snapshot(&mut PanicOnSnapshotAccess, 2, 1)
        .expect_err("the validated op count exceeds the configured atomic ceiling");
    assert!(
        error
            .to_string()
            .contains("op count 2 exceeds the configured maximum 1"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn ops_file_physical_line_and_total_caps_are_fail_closed() {
    let mut within = std::io::Cursor::new(b"1234567\n".to_vec());
    assert_eq!(
        read_bounded_ops_line_with_limit(&mut within, 1, 8)
            .unwrap()
            .unwrap(),
        "1234567"
    );
    let mut over = std::io::Cursor::new(b"12345678\n".to_vec());
    let error = read_bounded_ops_line_with_limit(&mut over, 7, 8).unwrap_err();
    assert!(error.to_string().contains("line 7"));

    let oversized = NamedTempFile::new().unwrap();
    oversized.as_file().set_len(MAX_OPS_FILE_BYTES + 1).unwrap();
    let error = validate_ops_file(oversized.path()).unwrap_err();
    assert!(error.to_string().contains("total limit"));
}

#[test]
fn large_ops_file_payload_is_read_from_path_not_argv() {
    let mut file = NamedTempFile::new().unwrap();
    let payload = "x".repeat(1024 * 1024);
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({"tool":"stats","args":{"payload":payload}}),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let path = file.path().to_str().unwrap();
    let args = ExecArgs::try_parse_from(["exec", "--ops-file", path]).unwrap();
    assert!(args.ops.is_none());
    assert_eq!(args.ops_file.as_deref(), Some(file.path()));
    assert_eq!(validate_ops_file(file.path()).unwrap().total, 1);
}

#[test]
fn chunk_byte_boundary_is_exact() {
    assert!(!should_defer_chunk_entry(
        0,
        0,
        OPS_FILE_CHUNK_MAX_BYTES + 1
    ));
    assert!(!should_defer_chunk_entry(
        1,
        OPS_FILE_CHUNK_MAX_BYTES - 1,
        1
    ));
    assert!(should_defer_chunk_entry(1, OPS_FILE_CHUNK_MAX_BYTES, 1));
}

#[test]
fn ordered_chunk_contract_rejects_tool_or_summary_drift() {
    let tools = vec![
        "first".to_string(),
        "second".to_string(),
        "third".to_string(),
    ];
    let valid = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first","result":{}},
            {"ok":false,"tool":"second","error":"no"},
            {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
        ],
        "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
        "status":"partial"
    });
    assert_eq!(
        validate_ordered_chunk_envelope(&tools, &valid, 1).unwrap(),
        (1, 1, 1)
    );

    let mut wrong_tool = valid.clone();
    wrong_tool["results"][1]["tool"] = serde_json::json!("third");
    assert!(validate_ordered_chunk_envelope(&tools, &wrong_tool, 1).is_err());

    let mut lying_summary = valid;
    lying_summary["summary"]["succeeded"] = serde_json::json!(2);
    lying_summary["summary"]["failed"] = serde_json::json!(0);
    assert!(validate_ordered_chunk_envelope(&tools, &lying_summary, 1).is_err());

    let mut missing_result = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first"},
            {"ok":false,"tool":"second","error":"no"},
            {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
        ],
        "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
        "status":"partial"
    });
    assert!(validate_ordered_chunk_envelope(&tools, &missing_result, 1).is_err());
    missing_result["results"][0]["result"] = serde_json::Value::Null;
    missing_result["results"][1]
        .as_object_mut()
        .unwrap()
        .remove("error");
    assert!(validate_ordered_chunk_envelope(&tools, &missing_result, 1).is_err());

    let mut contradictory = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first","result":null,"error":null},
            {"ok":false,"tool":"second","error":"no","result":null},
            {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
        ],
        "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
        "status":"partial"
    });
    assert!(validate_ordered_chunk_envelope(&tools, &contradictory, 1).is_err());
    contradictory["results"][0]
        .as_object_mut()
        .unwrap()
        .remove("error");
    assert!(validate_ordered_chunk_envelope(&tools, &contradictory, 1).is_err());
}

fn status_contract_fixture(status: &str) -> (Vec<String>, serde_json::Value) {
    (
        vec!["first".to_string(), "second".to_string()],
        serde_json::json!({
            "results": [
                {"ok":true,"tool":"first","result":{}},
                {"ok":false,"tool":"second","error":"no"}
            ],
            "summary":{"total":2,"succeeded":1,"failed":1,"aborted":0},
            "status":status
        }),
    )
}

#[test]
fn ordered_chunk_truthful_status_passes() {
    let (ops, envelope) = status_contract_fixture("partial");
    assert_eq!(
        validate_ordered_chunk_envelope(&ops, &envelope, 1).unwrap(),
        (1, 1, 0)
    );
}

#[test]
fn ordered_chunk_contradicting_status_is_rejected() {
    let (ops, envelope) = status_contract_fixture("success");
    let error = validate_ordered_chunk_envelope(&ops, &envelope, 1).unwrap_err();
    assert!(error.to_string().contains("status"));
}

// ── integration: bulk apply (isolated DB) ─────────────────────────────────

#[tokio::test]
async fn ops_file_applies_ops_and_summary_matches() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    // Write 3 create-entity ops.
    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    for name in ["Alpha", "Beta", "Gamma"] {
        let line = format!(
            "{{\"tool\":\"create\",\"args\":{{\"kind\":\"concept\",\"name\":\"{name}\"}}}}\n"
        );
        f.write_all(line.as_bytes()).unwrap();
    }

    let ops = parse_ops_file(f.path()).unwrap();
    assert_eq!(ops.len(), 3);
    let summary = apply_ops_file(&server, ops, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["succeeded"], 3);
    assert_eq!(summary["failed"], 0);
    assert!(summary.get("aborted").is_none());
    assert!(summary.get("failure_details_omitted").is_none());
    assert!(summary.get("results").is_none());

    // Verify all 3 entities are present.
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let count = resp["results"][0]["result"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        count, 3,
        "all 3 entities should be present after apply\nraw: {resp}"
    );
}

#[tokio::test]
async fn serial_ops_file_max_in_flight_is_one_while_default_remains_parallel() {
    let op_count = OPS_FILE_CHUNK_SIZE;
    let (parallel_server, parallel_in_flight, parallel_max) = concurrency_probe_server(false);
    let parallel_summary = apply_ops_file(
        &parallel_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
    )
    .await
    .expect("the ordinary bounded-parallel batch must succeed");
    assert_eq!(parallel_summary["succeeded"], op_count);
    let observed_parallel_max = parallel_max.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        observed_parallel_max, 8,
        "default dispatch must retain the server's bounded parallelism"
    );
    assert_eq!(
        parallel_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    let (serial_server, serial_in_flight, serial_max) = concurrency_probe_server(false);
    let serial_summary = apply_ops_file_with_dispatch_mode(
        &serial_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect("serial dispatch must succeed");
    assert_eq!(serial_summary["succeeded"], op_count);
    assert_eq!(
        serial_max.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "--serial must await every handler before starting the next"
    );
    assert_eq!(
        serial_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn serial_ops_file_succeeds_with_a_reader_that_refuses_overlap() {
    let op_count = 4;
    let (parallel_server, _, parallel_max) = concurrency_probe_server(true);
    let parallel_summary = apply_ops_file(
        &parallel_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
    )
    .await
    .expect("one parallel probe succeeds, so partial failure remains in-band");
    assert_eq!(parallel_summary["succeeded"], 1);
    assert_eq!(parallel_summary["failed"], op_count - 1);
    assert_eq!(
        parallel_max.load(std::sync::atomic::Ordering::SeqCst),
        op_count
    );
    assert!(parallel_summary["failures"]
        .as_array()
        .expect("failure rows")
        .iter()
        .all(|failure| failure["error"]["message"]
            .as_str()
            .expect("error.message is text")
            .contains("sql_bridge.reader_open")));

    let (serial_server, _, serial_max) = concurrency_probe_server(true);
    let serial_summary = apply_ops_file_with_dispatch_mode(
        &serial_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect("serial dispatch must avoid constrained-reader overlap");
    assert_eq!(serial_summary["succeeded"], op_count);
    assert_eq!(serial_summary["failed"], 0);
    assert_eq!(serial_max.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn serial_ops_file_preserves_order_save_rows_and_strict_failure() {
    let (server, _, max_in_flight) = concurrency_probe_server(false);
    let mut ops = reader_probe_ops(3);
    ops[1].args["fail"] = serde_json::json!(true);
    let output_dir = tempfile::tempdir().expect("output dir");
    let save_path = output_dir.path().join("serial-ordered.jsonl");

    let error = apply_ops_file_with_dispatch_mode(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect_err("strict mode must report the middle handler failure");
    assert!(error.to_string().contains("--strict"), "{error:#}");
    assert_eq!(max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);

    let rows: Vec<serde_json::Value> = std::fs::read_to_string(save_path)
        .expect("strict failure still publishes all confirmed ordered rows")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON result row"))
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["tool"], "reader_probe");
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[0]["result"]["sequence"], 0);
    assert_eq!(rows[1]["tool"], "reader_probe");
    assert_eq!(rows[1]["ok"], false);
    assert_eq!(rows[1]["reason"], "strict-op-failure");
    assert_eq!(rows[2]["tool"], "reader_probe");
    assert_eq!(rows[2]["ok"], true);
    assert_eq!(rows[2]["result"]["sequence"], 2);
}

#[tokio::test]
async fn serial_dispatch_consumes_the_preflighted_stable_snapshot() {
    use std::io::{Seek as _, Write as _};

    let mut source = NamedTempFile::new().expect("ops source");
    for sequence in 0..2 {
        serde_json::to_writer(
            source.as_file_mut(),
            &serde_json::json!({
                "tool": "reader_probe",
                "args": {"sequence": sequence},
            }),
        )
        .expect("write source op");
        source.write_all(b"\n").expect("write newline");
    }
    let mut validated = validate_ops_file(source.path()).expect("preflight source");

    source.as_file_mut().set_len(0).expect("truncate source");
    source
        .as_file_mut()
        .rewind()
        .expect("rewind replacement source");
    source
        .write_all(b"{malformed replacement\n")
        .expect("replace source after preflight");
    source.flush().expect("flush replacement");

    let (server, _, max_in_flight) = concurrency_probe_server(false);
    let summary = apply_ops_file_reader_with_dispatch_mode(
        &server,
        std::io::BufReader::new(&mut validated.snapshot),
        validated.total,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect("dispatch must consume the immutable validated snapshot");
    assert_eq!(summary["total"], 2);
    assert_eq!(summary["succeeded"], 2);
    assert_eq!(max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn serial_typed_preflight_rejects_later_prev_before_first_write() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write as _;

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let mut source = NamedTempFile::new().expect("ops source");
    source
        .write_all(
            b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"must-not-land\"}}\n",
        )
        .expect("write valid first op");
    source
        .write_all(b"{\"tool\":\"stats\",\"args\":{\"probe\":\"$prev.id\"}}\n")
        .expect("write typed-invalid later op");

    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("write empty config");
    let cfg = RuntimeConfig {
        db_path: Some(PathBuf::from(&db_path)),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let error = run_exec_ops_file(
        source.path().to_path_buf(),
        cfg,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(db_path.clone()),
            anchor: Some(PathBuf::from(&db_path)),
            config: Some(config_path),
        },
        true,
        false,
        None,
        false,
    )
    .await
    .expect_err("later $prev must fail before the first serial handler starts");
    assert!(error.to_string().contains("$prev"), "{error:#}");

    let server = isolated_server(&db_path);
    let response = dispatch_json(&server, r#"list(kind="concept")"#).await;
    assert_eq!(
        response["results"][0]["result"]["items"],
        serde_json::json!([]),
        "whole-snapshot typed preflight must prevent the valid first write"
    );
}

#[tokio::test]
async fn serial_whole_snapshot_preflight_does_not_change_default_chunk_commit_parity() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write as _;

    let mut source = NamedTempFile::new().expect("ops source");
    for sequence in 0..OPS_FILE_CHUNK_SIZE {
        serde_json::to_writer(
            source.as_file_mut(),
            &serde_json::json!({
                "tool": "create",
                "args": {
                    "kind": "concept",
                    "name": format!("default-first-chunk-{sequence}"),
                },
            }),
        )
        .expect("write valid first-chunk op");
        source.write_all(b"\n").expect("write newline");
    }
    source
        .write_all(b"{\"tool\":\"stats\",\"args\":{\"probe\":\"$prev.id\"}}\n")
        .expect("write typed-invalid second-chunk op");

    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let default_db = NamedTempFile::new().expect("default db");
    let default_db_path = default_db.path().to_str().expect("utf8").to_string();
    let default_error = run_exec_ops_file(
        source.path().to_path_buf(),
        RuntimeConfig {
            db_path: Some(PathBuf::from(&default_db_path)),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        },
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(default_db_path.clone()),
            anchor: Some(PathBuf::from(&default_db_path)),
            config: Some(config_path.clone()),
        },
        false,
        false,
        None,
        false,
    )
    .await
    .expect_err("default mode must reject the typed-invalid second chunk");
    assert!(
        default_error.to_string().contains("$prev"),
        "{default_error:#}"
    );
    let default_server = isolated_server(&default_db_path);
    let default_response =
        dispatch_json(&default_server, r#"list(kind="concept", limit=200)"#).await;
    assert_eq!(
        default_response["results"][0]["result"]["items"]
            .as_array()
            .expect("default concept rows")
            .len(),
        OPS_FILE_CHUNK_SIZE,
        "backward-compatible default mode must commit its first logical chunk"
    );

    let serial_db = NamedTempFile::new().expect("serial db");
    let serial_db_path = serial_db.path().to_str().expect("utf8").to_string();
    let serial_error = run_exec_ops_file(
        source.path().to_path_buf(),
        RuntimeConfig {
            db_path: Some(PathBuf::from(&serial_db_path)),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        },
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(serial_db_path.clone()),
            anchor: Some(PathBuf::from(&serial_db_path)),
            config: Some(config_path),
        },
        true,
        false,
        None,
        false,
    )
    .await
    .expect_err("serial mode must reject the later invalid op before dispatch");
    assert!(
        serial_error.to_string().contains("$prev"),
        "{serial_error:#}"
    );
    let serial_server = isolated_server(&serial_db_path);
    let serial_response = dispatch_json(&serial_server, r#"list(kind="concept", limit=200)"#).await;
    assert_eq!(
        serial_response["results"][0]["result"]["items"],
        serde_json::json!([]),
        "serial whole-snapshot preflight must reject before its first write"
    );
}

/// #3089 end-to-end witness: a daemonless write dispatched through
/// `run_exec_ops_file` must leave no `-wal`/`-shm` sidecar once it
/// returns. `run_exec_ops_file` never attempts daemon forwarding (see
/// its own doc comment), so this drives the real in-process dispatch
/// path deterministically without a spy or `KHIVE_NO_DAEMON`, and it
/// exercises the same `settle_exec_storage_before_return` helper that
/// `run_exec_inline_with_forward` calls at its own tail, including the
/// ADR-133 audit-batch drain (`with_mounted_packs`,
/// `khive-mcp/src/server.rs`, wires an `EventStore` for every
/// non-read-only runtime this test's writable config builds) and the
/// writer-task join. `pool_drop_never_leaves_a_reader_as_the_last_connection_closed`
/// (`khive-db/src/pool.rs`) already covers `ConnectionPool::drop` in
/// isolation; this test covers the caller that owns the process exit
/// path around it.
#[tokio::test]
async fn ops_file_write_leaves_no_wal_sidecar_after_return() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write as _;

    let db_dir = tempfile::tempdir().expect("db dir");
    let db_path = db_dir.path().join("close-order.db");
    let db_path_str = db_path.to_str().expect("utf8").to_string();
    let wal = {
        let mut name = db_path.file_name().unwrap().to_os_string();
        name.push("-wal");
        db_path.parent().unwrap().join(name)
    };
    let shm = {
        let mut name = db_path.file_name().unwrap().to_os_string();
        name.push("-shm");
        db_path.parent().unwrap().join(name)
    };

    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let mut source = NamedTempFile::new().expect("ops source");
    source
        .write_all(
            b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"wal-sidecar-witness\"}}\n",
        )
        .expect("write create op");

    let cfg = RuntimeConfig {
        db_path: Some(db_path.clone()),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };

    run_exec_ops_file(
        source.path().to_path_buf(),
        cfg,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(db_path_str),
            anchor: Some(db_path),
            config: Some(config_path),
        },
        false,
        false,
        None,
        false,
    )
    .await
    .expect("ops-file write must succeed");

    assert!(
        !wal.exists(),
        "a daemonless write through run_exec_ops_file must not leave a -wal sidecar behind after return"
    );
    assert!(
        !shm.exists(),
        "a daemonless write through run_exec_ops_file must not leave a -shm sidecar behind after return"
    );
}

#[tokio::test]
async fn oversized_single_ops_file_reaches_handler_validation() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let ops = vec![OpsFileEntry {
        tool: "stats".to_string(),
        args: serde_json::json!({
            "payload": "x".repeat(khive_request::MAX_OPS_INPUT_LEN + 1),
        }),
    }];
    let mut observed_handler_error = false;

    let error = apply_ops_file_with_response_transform(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        |_, raw| {
            let response: serde_json::Value =
                serde_json::from_str(&raw).expect("handler response must be JSON");
            assert_eq!(response["results"][0]["tool"], "stats");
            assert_eq!(response["results"][0]["ok"], false);
            assert!(
                response["results"][0]["error"]
                    .to_string()
                    .contains("payload"),
                "the oversized typed op must reach stats argument validation: {response}"
            );
            observed_handler_error = true;
            raw
        },
    )
    .await
    .expect_err("the only op is intentionally invalid at the handler boundary");

    assert!(
        observed_handler_error,
        "ops-file dispatch must not reapply the public 1 MiB raw-DSL limit"
    );
    assert!(error.to_string().contains("every op failed"));
}

#[tokio::test]
async fn oversized_multi_op_chunk_preserves_order_save_and_strict() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let payload = "x".repeat(600 * 1024);
    let ops = vec![
        OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({
                "kind": "concept",
                "name": "oversized ordered success",
                "description": payload,
            }),
        },
        OpsFileEntry {
            tool: "stats".to_string(),
            args: serde_json::json!({
                "payload": "y".repeat(600 * 1024),
            }),
        },
    ];
    let encoded_len = serde_json::to_vec(&ops).unwrap().len();
    assert!(encoded_len > khive_request::MAX_OPS_INPUT_LEN);
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("oversized-ordered.jsonl");

    let error = apply_ops_file(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
    )
    .await
    .expect_err("strict mode must report the handler-level stats failure");

    assert!(error.to_string().contains("--strict"), "{error:#}");
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&save_path)
        .expect("strict failure still publishes the complete ordered result file")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["tool"], "create");
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[1]["tool"], "stats");
    assert_eq!(rows[1]["ok"], false);
    assert_eq!(rows[1]["reason"], "strict-op-failure");
}

#[tokio::test]
async fn public_dispatch_still_rejects_oversized_raw_ops() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let params = RequestParams {
        plan: None,
        ops: serde_json::json!({
            "tool": "stats",
            "args": {"payload": "x".repeat(khive_request::MAX_OPS_INPUT_LEN + 1)},
        })
        .to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: Some("json".to_string()),
        format_per_op: None,
        request_id: None,
    };

    let error = server
        .dispatch_request_local(params)
        .await
        .expect_err("the raw request surface must retain its 1 MiB safety bound");
    assert!(
        error.to_string().contains("ops input is")
            && error.to_string().contains(&format!(
                "max is {} bytes",
                khive_request::MAX_OPS_INPUT_LEN
            )),
        "unexpected public dispatch error: {error}"
    );
}

#[tokio::test]
async fn multi_chunk_save_retains_order_rows_checksum_and_json_override() {
    use sha2::Digest as _;

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server =
        isolated_server(&db_path).with_default_output_format(khive_runtime::OutputFormat::Table);
    let ops: Vec<OpsFileEntry> = (0..=OPS_FILE_CHUNK_SIZE)
        .map(|index| OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({
                "kind": "concept",
                "name": format!("ordered-{index:03}"),
            }),
        })
        .collect();
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("ordered.jsonl");

    let manifest = apply_ops_file(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
    )
    .await
    .unwrap();

    let manifest_keys: std::collections::BTreeSet<_> = manifest
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        manifest_keys,
        std::collections::BTreeSet::from([
            "checksum",
            "path",
            "per_column_null_counts",
            "rows",
            "schema_fingerprint",
            "summary",
        ]),
        "the successful manifest shape must remain unchanged"
    );
    assert_eq!(manifest["rows"], OPS_FILE_CHUNK_SIZE + 1);
    assert_eq!(manifest["summary"]["total"], OPS_FILE_CHUNK_SIZE + 1);
    assert_eq!(manifest["summary"]["succeeded"], OPS_FILE_CHUNK_SIZE + 1);
    assert_eq!(manifest["summary"]["failed"], 0);
    assert_eq!(manifest["summary"]["aborted"], 0);

    let bytes = std::fs::read(&save_path).unwrap();
    let checksum = format!("{:x}", sha2::Sha256::digest(&bytes));
    assert_eq!(manifest["checksum"], checksum);
    let rows: Vec<serde_json::Value> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(rows.len(), OPS_FILE_CHUNK_SIZE + 1);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["tool"], "create");
        assert_eq!(row["ok"], true);
        assert_eq!(row["result"]["name"], format!("ordered-{index:03}"));
    }
}

#[tokio::test]
async fn malformed_later_chunk_emits_aborted_manifest_for_prior_commits() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let ops: Vec<OpsFileEntry> = (0..=OPS_FILE_CHUNK_SIZE)
        .map(|index| OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({
                "kind": "concept",
                "name": format!("abort-manifest-{index:03}"),
            }),
        })
        .collect();
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("must-not-publish.jsonl");

    let error = apply_ops_file_with_response_transform(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
        |chunk_number, raw| {
            if chunk_number == 2 {
                "{malformed-response".to_string()
            } else {
                raw
            }
        },
    )
    .await
    .unwrap_err();

    let aborted = error
        .downcast_ref::<AbortedOpsFileError>()
        .expect("post-dispatch failure must carry its emitted manifest");
    assert_eq!(aborted.manifest["status"], "aborted");
    assert_eq!(aborted.manifest["committed_chunks"], serde_json::json!([1]));
    assert_eq!(aborted.manifest["dispatched_chunk"], 2);
    assert_eq!(aborted.manifest["file_published"], false);
    assert_eq!(
        aborted.manifest["summary"]["succeeded"],
        OPS_FILE_CHUNK_SIZE
    );
    assert_eq!(aborted.manifest["summary"]["total"], OPS_FILE_CHUNK_SIZE);
    assert_eq!(aborted.manifest["summary"]["aborted"], 0);
    assert_eq!(aborted.manifest["unconfirmed_ops"], 1);
    assert!(
        !save_path.exists(),
        "an aborted run must not publish partial JSONL"
    );

    // `committed_chunks: [1]` is a claim about DURABLE STATE, and the manifest
    // asserting it is assembled locally. Every assertion above would still pass
    // if chunk 1's writes had been rolled back or never reached storage, because
    // the bookkeeping would simply agree with itself. Read it back through the
    // same server so the reconciliation record is checked against the database
    // it describes. The stable list contract wraps rows in `items` whether or
    // not the requested limit reaches the entity cap.
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept", limit=200)"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: Some("json".to_string()),
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let rows = response["results"][0]["result"]["items"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "read-back list result must contain an items array; got {}",
                response["results"][0]["result"]
            )
        });
    // A row that carries no string name is an unreadable instrument, not an
    // absent entity, so it panics here rather than being dropped silently.
    let mut observed: Vec<String> = rows
        .iter()
        .map(|row| {
            row["name"]
                .as_str()
                .unwrap_or_else(|| panic!("read-back row carries no string name: {row}"))
                .to_owned()
        })
        .collect();
    // An empty or unparsed read-back is an instrument failure, not a pass.
    assert!(
        !observed.is_empty(),
        "read-back yielded no rows; result was {}",
        response["results"][0]["result"]
    );
    observed.sort_unstable();

    // Enumerate the two legal outcomes instead of bounding a count. Entity
    // names carry no uniqueness constraint and the upsert is keyed by UUID,
    // so any count of distinct names is a proxy: a duplicate row satisfies
    // it while the property it stands for is broken. Comparing the whole
    // sorted list pins which rows are present, and how many of each.
    let committed: Vec<String> = (0..OPS_FILE_CHUNK_SIZE)
        .map(|index| format!("abort-manifest-{index:03}"))
        .collect();
    // Chunk 2 was dispatched without a verified response, so its single op
    // may or may not have landed. The manifest reports it as unconfirmed
    // rather than committed precisely because both outcomes are legal here.
    let mut with_unconfirmed = committed.clone();
    with_unconfirmed.push(format!("abort-manifest-{OPS_FILE_CHUNK_SIZE:03}"));

    assert!(
        observed == committed || observed == with_unconfirmed,
        "manifest reports chunk 1 committed and chunk 2 unconfirmed, so the database must \
             hold exactly the {OPS_FILE_CHUNK_SIZE} confirmed rows, optionally plus the one \
             unconfirmed row; found {} rows: {observed:?}",
        observed.len()
    );
}

#[tokio::test]
async fn invalid_save_directory_is_rejected_before_any_op_side_effect() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let output_dir = tempfile::tempdir().unwrap();
    let ops = vec![OpsFileEntry {
        tool: "create".to_string(),
        args: serde_json::json!({"kind":"concept","name":"must-not-exist"}),
    }];

    let error = apply_ops_file(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(output_dir.path().to_string_lossy().into_owned()),
        true,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("absent or an existing regular file"));

    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: Some("json".to_string()),
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        response["results"][0]["result"]["items"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn save_manifest_preserves_partial_summary_and_strict_writes_rows() {
    fn partial_ops(success_name: &str) -> Vec<OpsFileEntry> {
        vec![
            OpsFileEntry {
                tool: "create".to_string(),
                args: serde_json::json!({"kind":"concept","name":success_name}),
            },
            OpsFileEntry {
                tool: "search".to_string(),
                args: serde_json::json!({"kind":"not_a_real_kind","query":"x"}),
            },
        ]
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("partial.jsonl");
    let manifest = apply_ops_file(
        &server,
        partial_ops("partial-ok"),
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(manifest["rows"], 2);
    assert_eq!(manifest["summary"]["succeeded"], 1);
    assert_eq!(manifest["summary"]["failed"], 1);
    assert_eq!(manifest["summary"]["aborted"], 0);

    let strict_path = output_dir.path().join("strict-partial.jsonl");
    let error = apply_ops_file(
        &server,
        partial_ops("strict-partial-ok"),
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(strict_path.to_string_lossy().into_owned()),
        true,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("--strict"));
    assert_eq!(
        std::fs::read_to_string(strict_path)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

// ── #1220: --strict exit-code signal for partially-failed batches ─────────

#[test]
fn prepare_exec_output_preserves_specific_reasons_and_fills_strict_failures() {
    let raw = serde_json::json!({
        "results": [
            {"ok": true, "tool": "stats", "result": {}},
            {"ok": false, "tool": "get", "error": "missing id"},
            {
                "ok": false,
                "tool": "not_loaded",
                "error": "unknown verb",
                "reason": "verb-refused"
            },
            {"ok": false, "tool": "update", "aborted": true},
        ],
        "summary": {"total": 4, "succeeded": 1, "failed": 2, "aborted": 1},
        "status": "partial",
    })
    .to_string();

    let parsed: serde_json::Value = serde_json::from_str(&prepare_exec_output(&raw, true)).unwrap();
    assert_eq!(parsed["results"][1]["reason"], "strict-op-failure");
    assert_eq!(parsed["results"][2]["reason"], "verb-refused");
    assert_eq!(parsed["results"][3]["reason"], "strict-op-failure");
    assert!(parsed["results"][0].get("reason").is_none());
}

#[test]
fn enforce_strict_batch_result_ok_when_strict_off_and_partially_failed() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, false).is_ok());
}

// ── #1339: fully-failed batches exit non-zero even without --strict ──────

#[test]
fn enforce_strict_batch_result_errs_when_strict_off_and_every_op_failed() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 1, "succeeded": 0, "failed": 1, "aborted": 0},
    })
    .to_string();
    let err = enforce_strict_batch_result(&raw, false).unwrap_err();
    assert!(format!("{err}").contains("every op failed"));
}

#[test]
fn enforce_strict_batch_result_errs_when_strict_off_and_chain_fully_aborted() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 3, "succeeded": 0, "failed": 1, "aborted": 2},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, false).is_err());
}

#[test]
fn enforce_strict_batch_result_ok_on_empty_batch_summary() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 0, "succeeded": 0, "failed": 0, "aborted": 0},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, false).is_ok());
    assert!(enforce_strict_batch_result(&raw, true).is_ok());
}

#[test]
fn enforce_strict_batch_result_ok_when_strict_on_and_nothing_failed() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 2, "failed": 0, "aborted": 0},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, true).is_ok());
}

#[test]
fn enforce_strict_batch_result_errs_when_strict_on_and_a_failure_present() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
    })
    .to_string();
    let err = enforce_strict_batch_result(&raw, true).unwrap_err();
    assert!(format!("{err}").contains("1 op(s) failed"));
}

#[test]
fn enforce_strict_batch_result_errs_when_strict_on_and_chain_aborted() {
    let raw = serde_json::json!({
        "results": [],
        "summary": {"total": 2, "succeeded": 0, "failed": 1, "aborted": 1},
    })
    .to_string();
    assert!(enforce_strict_batch_result(&raw, true).is_err());
}

#[test]
fn enforce_strict_batch_result_errs_on_save_manifest_with_failures() {
    // The save-file path prints a manifest, not the raw envelope; the
    // manifest carries the envelope's summary through (khive-mcp
    // save_sink) precisely so --strict works on this path too.
    let raw = r#"{"path":"/tmp/out.jsonl","rows":2,"checksum":"ab","summary":{"total":2,"succeeded":1,"failed":1,"aborted":0}}"#;
    assert!(enforce_strict_batch_result(raw, true).is_err());
    let clean = r#"{"path":"/tmp/out.jsonl","rows":2,"checksum":"ab","summary":{"total":2,"succeeded":2,"failed":0,"aborted":0}}"#;
    assert!(enforce_strict_batch_result(clean, true).is_ok());
}

#[test]
fn enforce_strict_batch_result_ok_on_non_json_output() {
    // --output-format table/auto renders a non-JSON string; --strict has
    // nothing to inspect and must not itself error out on that shape.
    assert!(enforce_strict_batch_result("| a | b |\n", true).is_ok());
}

#[tokio::test]
async fn apply_ops_file_strict_errs_when_an_op_fails() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    // First op succeeds; second targets an unknown kind and fails.
    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"StrictOne\"}}\n")
        .unwrap();
    f.write_all(b"{\"tool\":\"search\",\"args\":{\"kind\":\"not_a_real_kind\",\"query\":\"x\"}}\n")
        .unwrap();

    let ops = parse_ops_file(f.path()).unwrap();
    assert_eq!(ops.len(), 2);

    let err = apply_ops_file(&server, ops, None, None, None, true)
        .await
        .expect_err("strict mode must surface the per-op failure as a process error");
    assert!(format!("{err}").contains("1 op(s) failed"));
}

#[tokio::test]
async fn apply_ops_file_errs_without_strict_when_every_op_fails() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"tool\":\"search\",\"args\":{\"kind\":\"not_a_real_kind\",\"query\":\"x\"}}\n")
        .unwrap();

    let ops = parse_ops_file(f.path()).unwrap();
    let err = apply_ops_file(&server, ops, None, None, None, false)
        .await
        .expect_err("a fully-failed ops-file must exit non-zero even without --strict");
    assert!(format!("{err}").contains("every op failed"));
}

// ── ADR-099 B1 inertness (golden shape) ────────────────────────────────────
//
// B1 adds only new, unconsumed types (khive-types atomic admissibility
// metadata, khive-runtime atomic-plan data, khive-request's parse-time
// check). None of them are wired into `dispatch_request_local` or
// `apply_ops_file` — this test pins the non-atomic response envelope's
// shape so a later slice that DOES wire `--atomic` in cannot silently
// change today's default (non-atomic) output. The op sequence below
// (create → update → link → get) is the representative mix named in the
// task: a create, a mutation, a graph edge, and a read, run back-to-back
// through the same in-process dispatch path bulk apply uses.
#[tokio::test]
async fn non_atomic_dispatch_envelope_shape_is_unchanged_by_adr099_b1() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    async fn dispatch(server: &KhiveMcpServer, ops: &str) -> serde_json::Value {
        let params = RequestParams {
            plan: None,
            ops: ops.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        };
        let raw = server
            .dispatch_request_local(params)
            .await
            .unwrap_or_else(|e| panic!("dispatch {ops:?} failed: {e}"));
        serde_json::from_str(&raw).expect("valid JSON")
    }

    // create
    let created = dispatch(
        &server,
        r#"create(kind="concept", name="ADR-099-B1-inertness")"#,
    )
    .await;
    assert_golden_envelope_shape(&created, "create");
    let entity_id = created["results"][0]["result"]["id"]
        .as_str()
        .expect("create must return an id")
        .to_string();

    // update
    let updated = dispatch(
        &server,
        &format!(r#"update(id="{entity_id}", description="updated by inertness test")"#),
    )
    .await;
    assert_golden_envelope_shape(&updated, "update");

    // link (self-referential edge is rejected by endpoint validation for
    // most relations, so create a second entity as the link target)
    let target = dispatch(&server, r#"create(kind="concept", name="link-target")"#).await;
    let target_id = target["results"][0]["result"]["id"]
        .as_str()
        .expect("create must return an id")
        .to_string();
    let linked = dispatch(
        &server,
        &format!(r#"link(source_id="{entity_id}", target_id="{target_id}", relation="extends")"#),
    )
    .await;
    assert_golden_envelope_shape(&linked, "link");

    // get (read)
    let got = dispatch(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert_golden_envelope_shape(&got, "get");

    // Every op above succeeded end-to-end with zero surprises in the
    // envelope shape — this is the inertness pin: no `atomic` key
    // appeared anywhere, `summary` kept exactly its 4 pre-existing
    // fields on every response, and every op's own result still nests
    // under `results[0].result` as before.
}

/// Asserts a `dispatch_request_local` response matches the pre-ADR-099-B1
/// golden shape: exactly the top-level keys `results` and `summary` (no
/// additive `atomic` block — that is a future, opt-in-only slice), a
/// `summary` with exactly `total`/`succeeded`/`failed`/`aborted`, and a
/// successful single-op `results[0]` carrying `ok`/`tool`/`result`.
fn assert_golden_envelope_shape(resp: &serde_json::Value, expected_tool: &str) {
    let top_level_keys: std::collections::BTreeSet<&str> = resp
        .as_object()
        .expect("response must be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        top_level_keys,
        std::collections::BTreeSet::from(["results", "summary", "status"]),
        "non-atomic envelope must carry exactly results+summary+status, no `atomic` block (#1220 added `status`): {resp}"
    );

    let summary_keys: std::collections::BTreeSet<&str> = resp["summary"]
        .as_object()
        .expect("summary must be an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        summary_keys,
        std::collections::BTreeSet::from(["total", "succeeded", "failed", "aborted"]),
        "summary shape must be unchanged: {resp}"
    );
    assert_eq!(resp["summary"]["total"], serde_json::json!(1));
    assert_eq!(resp["summary"]["succeeded"], serde_json::json!(1));
    assert_eq!(resp["summary"]["failed"], serde_json::json!(0));

    assert_eq!(resp["results"][0]["ok"], serde_json::json!(true));
    assert_eq!(resp["results"][0]["tool"], serde_json::json!(expected_tool));
    assert!(
        resp["results"][0].get("result").is_some(),
        "results[0] must carry a `result` field: {resp}"
    );
}

#[tokio::test]
async fn ops_file_dry_run_writes_nothing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    for name in ["DryA", "DryB"] {
        let line = format!(
            "{{\"tool\":\"create\",\"args\":{{\"kind\":\"concept\",\"name\":\"{name}\"}}}}\n"
        );
        f.write_all(line.as_bytes()).unwrap();
    }

    let path = f.path().to_path_buf();
    let cfg = RuntimeConfig {
        db_path: Some(PathBuf::from(&db_path)),
        ..Default::default()
    };

    // dry_run=true → no writes.
    run_exec_ops_file(
        path.clone(),
        cfg.clone(),
        None,
        None,
        None,
        true,
        ExecDbContext::default(),
        false,
        false,
        None,
        false,
    )
    .await
    .unwrap();

    // Verify nothing was written by checking with a fresh server.
    let server = isolated_server(&db_path);
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let count = resp["results"][0]["result"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(count, 0, "dry-run must not write any entities");
}

#[tokio::test]
async fn atomic_dry_run_uses_the_real_read_only_admission() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("dry-run-target.db");
    let config_path = dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("empty config");
    let source_path = dir.path().join("ops.jsonl");
    std::fs::write(
        &source_path,
        "{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"MustNotLand\"}}\n",
    )
    .expect("ops file");
    let db_text = db_path.to_str().expect("utf8 db path").to_string();
    let context = || ExecDbContext {
        raw: Some(db_text.clone()),
        anchor: Some(db_path.clone()),
        config: Some(config_path.clone()),
    };

    for dry_run in [true, false] {
        let error = run_exec_ops_file(
            source_path.clone(),
            atomic_cfg(&db_text),
            None,
            None,
            None,
            dry_run,
            context(),
            false,
            true,
            None,
            false,
        )
        .await
        .expect_err("create is inadmissible in an atomic unit");
        assert!(
            error
                .downcast_ref::<crate::atomic_apply::AtomicExecFailure>()
                .is_some(),
            "dry_run={dry_run}: {error:#}"
        );
        assert!(!db_path.exists(), "preflight must not open the target db");
    }

    let over_limit = run_exec_ops_file(
        source_path,
        atomic_cfg(&db_text),
        None,
        None,
        None,
        true,
        context(),
        false,
        true,
        Some(0),
        false,
    )
    .await
    .expect_err("atomic dry-run must apply the configured op ceiling");
    assert!(over_limit.to_string().contains("op count 1"));
    assert!(
        !db_path.exists(),
        "over-limit preview must not open storage"
    );
}

#[derive(Debug)]
struct DenyPinnedActorGate {
    observed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl khive_runtime::Gate for DenyPinnedActorGate {
    fn check(
        &self,
        req: &khive_runtime::GateRequest,
    ) -> std::result::Result<khive_runtime::GateDecision, khive_runtime::GateError> {
        self.observed.lock().unwrap().push(req.actor.id.clone());
        if req.actor.id == "lambda:pinned" {
            Ok(khive_runtime::GateDecision::deny(
                "test actor is not granted",
            ))
        } else {
            Ok(khive_runtime::GateDecision::allow())
        }
    }
}

#[tokio::test]
#[serial]
async fn unauthorized_explicit_actor_is_not_retried_as_fallback() {
    if crate::test_process::run_in_child() {
        return;
    }

    let previous_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let (previous_home, _home_dir) = isolate_home_for_test();
    let db_file = NamedTempFile::new().expect("temp db");
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut cfg = RuntimeConfig {
        db_path: Some(db_file.path().to_path_buf()),
        actor_id: Some("lambda:fallback".to_string()),
        gate: std::sync::Arc::new(DenyPinnedActorGate {
            observed: observed.clone(),
        }),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    apply_actor_pin_and_expectation(&mut cfg, Some("lambda:pinned"), Some("lambda:pinned"))
        .unwrap();

    let result = run_exec_inline(
        r#"create(kind="concept", name="MustNotExist")"#.to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
    )
    .await;

    match previous_no_daemon {
        Some(value) => std::env::set_var("KHIVE_NO_DAEMON", value),
        None => std::env::remove_var("KHIVE_NO_DAEMON"),
    }
    restore_home(previous_home);

    assert!(result.is_err(), "the gate refusal must be terminal");
    let observed = observed.lock().unwrap();
    assert!(
        !observed.is_empty(),
        "the configured gate must be consulted"
    );
    assert!(
        observed.iter().all(|actor| actor == "lambda:pinned"),
        "no gate check may retry as the displaced fallback actor: {observed:?}"
    );
}

// ── strict-actor mode: daemon bypass regression ───────────────────────────

/// Security regression: strict-actor gate must fire before daemon forward.
/// See `crates/kkernel/docs/design.md#execrs-regression-test-notes`.
#[tokio::test]
#[serial]
async fn strict_mode_rejects_before_daemon_forward_when_comm_and_no_actor() {
    if crate::test_process::run_in_child() {
        return;
    }

    let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
    let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();

    std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
    // Belt-and-suspenders: ensure no daemon is contacted even if one happens
    // to be running.  The error should fire before forwarding, but we make the
    // test deterministic by also suppressing the daemon path.
    std::env::set_var("KHIVE_NO_DAEMON", "1");

    let cfg = RuntimeConfig {
        db_path: None, // in-memory
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: None, // no actor — triggers the strict-mode gate
        ..RuntimeConfig::default()
    };

    let result = run_exec_inline(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
    )
    .await;

    // Restore env.
    match prev_strict {
        Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
        None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
    }
    match prev_no_daemon {
        Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
        None => std::env::remove_var("KHIVE_NO_DAEMON"),
    }

    assert!(
        result.is_err(),
        "run_exec_inline must return Err under strict mode + comm + no actor; got Ok"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        "error must name the strict-mode env var; got: {msg}"
    );
    assert!(
        msg.contains("KHIVE_ACTOR"),
        "error must name the remedy (KHIVE_ACTOR); got: {msg}"
    );
}

/// Complement: strict mode must NOT reject when comm is loaded and an actor
/// IS configured — the daemon fast-path must remain available in that case.
#[tokio::test]
#[serial]
async fn strict_mode_allows_exec_when_comm_and_actor_configured() {
    if crate::test_process::run_in_child() {
        return;
    }

    let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
    let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
    let (prev_home, _home_dir) = isolate_home_for_test();

    std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
    std::env::set_var("KHIVE_NO_DAEMON", "1"); // force in-process to avoid daemon dep

    let cfg = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: Some("lambda:tenant-x".to_string()), // actor configured → no gate
        ..RuntimeConfig::default()
    };

    // The strict gate must pass; the actual dispatch will succeed (stats() is safe).
    let result = run_exec_inline(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
    )
    .await;

    match prev_strict {
        Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
        None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
    }
    match prev_no_daemon {
        Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
        None => std::env::remove_var("KHIVE_NO_DAEMON"),
    }
    restore_home(prev_home);

    assert!(
        result.is_ok(),
        "run_exec_inline must succeed under strict mode when actor IS configured; got: {result:?}"
    );
}

/// Default-off regression: when KHIVE_REQUIRE_ATTRIBUTED_ACTOR is unset,
/// run_exec_inline must NOT reject even with comm + no actor (OSS default path).
#[tokio::test]
#[serial]
async fn strict_mode_off_exec_inline_passes_with_comm_no_actor() {
    if crate::test_process::run_in_child() {
        return;
    }

    let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
    let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
    let (prev_home, _home_dir) = isolate_home_for_test();

    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"); // default OFF
    std::env::set_var("KHIVE_NO_DAEMON", "1");

    let cfg = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: None,
        ..RuntimeConfig::default()
    };

    let result = run_exec_inline(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
    )
    .await;

    match prev_strict {
        Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
        None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
    }
    match prev_no_daemon {
        Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
        None => std::env::remove_var("KHIVE_NO_DAEMON"),
    }
    restore_home(prev_home);

    assert!(
        result.is_ok(),
        "run_exec_inline must NOT reject when strict mode is OFF (OSS default); got: {result:?}"
    );
}

// ── adapter-boundary regression (cross-crate member) ──────────────────────
//
// The exec-side `forward_or_spawn_boxed` in this file converts `cfg.packs`
// into `Some(&packs)` at its own `forward_or_spawn_with_config_and_packs` call site
// (line ~249). Every spy-based test in this file replaces
// `forward_or_spawn_boxed` itself via `run_exec_inline_with_forward`'s
// `ForwardFnPtr` seam, so none of them execute that conversion. This test
// instead drives `run_exec_inline` — the real production entry point,
// which always calls the real `forward_or_spawn_boxed` on Unix — and
// observes the argument via a one-shot capture hook armed at the entry of
// `khive_mcp::daemon::forward_or_spawn_with_config_and_packs` itself, reached
// cross-crate via khive-mcp's `test-forward-seam` feature (enabled from
// this crate's `[dev-dependencies]` re-declaration of khive-mcp).
// Changing the adapter's `Some(&packs)` argument to `None` reddens this
// test.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn resolved_pack_list_reaches_real_exec_adapter_boundary() {
    if crate::test_process::run_in_child() {
        return;
    }

    let (prev_home, _home_dir) = isolate_home_for_test();

    khive_mcp::daemon::test_forward_seam::arm();

    let cfg = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string(), "gtd".to_string()],
        actor_id: None,
        ..RuntimeConfig::default()
    };

    let result = run_exec_inline(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
    )
    .await;

    restore_home(prev_home);

    assert!(
        result.is_ok(),
        "the intercepted dispatch through the real production entry point must \
             succeed: {result:?}"
    );
    assert_eq!(
        khive_mcp::daemon::test_forward_seam::take_captured(),
        Some(Some(vec!["kg".to_string(), "gtd".to_string()])),
        "the real forward_or_spawn_boxed adapter in this crate must convert cfg.packs \
             into Some(&packs) at the forward_or_spawn_with_config_and_packs call boundary"
    );
}

// ── spy-based isomorphism guard (Unix only) ───────────────────────────────
//
// The three tests above use KHIVE_NO_DAEMON=1, which disables the daemon
// fast-path at the `forward_or_spawn` level.  That makes them correct checks
// of the strict gate in isolation, but tautological w.r.t. the daemon-bypass
// bug: moving `enforce_strict_actor_mode` back to BELOW the daemon block would
// NOT cause those tests to fail because the daemon path is suppressed.
//
// These tests use `run_exec_inline_with_forward` directly, passing a spy
// function pointer.  KHIVE_NO_DAEMON is NOT set in the rejection test.
// The spy can therefore be reached if — and only if — `enforce_strict_actor_mode`
// is called AFTER the forwarding attempt.  Under the correct implementation
// (enforce first) the gate rejects before the spy is invoked, so the spy
// thread-local remains false.
//
// ISOMORPHISM PROOF:
//   Temporarily moved `enforce_strict_actor_mode` to below the daemon block in
//   `run_exec_inline_with_forward`.  `strict_mode_spy_confirms_enforce_fires_before_forward`
//   failed with: "spy forward_fn was called — enforce fired after forwarding"
//   Restoring the early check made the test pass again.
//   This confirms the test is NOT tautological w.r.t. the bug it guards.

// Thread-local spy flag shared between the outer test body and the spy fn pointer.
// Using a module-level thread_local! avoids the "two separate statics" trap that
// arises when thread_local! is declared inside a function body.
#[cfg(unix)]
std::thread_local! {
    static SPY_WAS_CALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(unix)]
fn spy_forward_records_call<'a>(
    _frame: &'a DaemonRequestFrame,
    _config: Option<PathBuf>,
    _db: Option<&'a str>,
    _packs: Vec<String>,
) -> super::ForwardFuture<'a> {
    SPY_WAS_CALLED.with(|c| c.set(true));
    Box::pin(async { None })
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn strict_mode_spy_confirms_enforce_fires_before_forward() {
    if crate::test_process::run_in_child() {
        return;
    }

    let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
    // Deliberately do NOT set KHIVE_NO_DAEMON — the spy must be reachable
    // if the enforce call is in the wrong place.
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
    SPY_WAS_CALLED.with(|c| c.set(false));

    let cfg = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: None, // no actor — should trigger the strict gate
        ..RuntimeConfig::default()
    };

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None, // output_format
        None,
        ExecDbContext::default(),
        false,
        spy_forward_records_call,
    )
    .await;

    let spy_was_called = SPY_WAS_CALLED.with(|c| c.get());
    SPY_WAS_CALLED.with(|c| c.set(false)); // clean up

    match prev_strict {
        Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
        None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
    }

    assert!(
        result.is_err(),
        "strict mode + comm + no actor must return Err; got Ok"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        "error must name the strict-mode env var; got: {msg}"
    );
    assert!(
        !spy_was_called,
        "spy forward_fn was called — enforce_strict_actor_mode fired AFTER forwarding, not before"
    );
}

/// Complement: when an actor IS configured, the spy fn is reached because
/// the gate passes and forwarding is attempted.  We use KHIVE_NO_DAEMON=1 so
/// the spy returns None and in-process dispatch handles the request normally.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn strict_mode_spy_forward_reached_when_actor_configured() {
    if crate::test_process::run_in_child() {
        return;
    }

    let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
    let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
    let (prev_home, _home_dir) = isolate_home_for_test();
    std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
    // Suppress real daemon; spy still records the call before returning None.
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    SPY_WAS_CALLED.with(|c| c.set(false));

    let cfg = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: Some("lambda:tenant-x".to_string()), // gate should pass
        ..RuntimeConfig::default()
    };

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None, // output_format
        None,
        ExecDbContext::default(),
        false,
        spy_forward_records_call,
    )
    .await;

    let spy_was_called = SPY_WAS_CALLED.with(|c| c.get());
    SPY_WAS_CALLED.with(|c| c.set(false));

    match prev_strict {
        Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
        None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
    }
    match prev_no_daemon {
        Some(v) => std::env::set_var("KHIVE_NO_DAEMON", v),
        None => std::env::remove_var("KHIVE_NO_DAEMON"),
    }
    restore_home(prev_home);

    assert!(
        result.is_ok(),
        "gate must pass when actor is configured; got: {result:?}"
    );
    assert!(
        spy_was_called,
        "spy forward_fn must be called when gate passes (KHIVE_NO_DAEMON=1 causes in-process fallback)"
    );
}

// ── D1-R3 (end-to-end): exec frame config_id vs. daemon config_id ────────
//
// `exec_config_id_matches_serve_config_id_for_multi_backend_topology` above
// proves `compute_config_id` folds the topology identically for exec-shaped
// and serve-shaped `RuntimeConfig`s — but it constructs both arms manually
// and never calls `run_exec_inline_with_forward` itself, so it would not
// notice a revert of the actual `compute_config_id(&cfg, Some(&khive_cfg))`
// call at the real call site above. This test closes that gap: it drives
// `run_exec_inline_with_forward` for real, against a project-local
// `.khive/config.toml` that declares a genuine multi-backend topology, and
// captures the DAEMON REQUEST FRAME's actual `config_id` via a spy — the
// exact value that would be sent over the wire to a real daemon.

#[cfg(unix)]
std::thread_local! {
    static SPY_CAPTURED_CONFIG_ID: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static SPY_CAPTURED_CONFIG_PATH: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static SPY_CAPTURED_DB: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static SPY_CAPTURED_PACKS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn spy_capture_config_id<'a>(
    frame: &'a DaemonRequestFrame,
    config: Option<PathBuf>,
    db: Option<&'a str>,
    packs: Vec<String>,
) -> super::ForwardFuture<'a> {
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = config);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = db.map(str::to_string));
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = Some(packs));
    Box::pin(async { None })
}

#[cfg(unix)]
fn spy_capture_config_and_succeed<'a>(
    frame: &'a DaemonRequestFrame,
    config: Option<PathBuf>,
    db: Option<&'a str>,
    packs: Vec<String>,
) -> super::ForwardFuture<'a> {
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = config);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = db.map(str::to_string));
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = Some(packs));
    Box::pin(async {
        Some(Ok(
            r#"{"results":[{"ok":true,"tool":"stats","result":{}}],"summary":{"total":1,"succeeded":1,"failed":0}}"#
                .to_string(),
        ))
    })
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn explicit_config_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);

    let dir = tempfile::tempdir().expect("config tempdir");
    let config_path = dir.path().join("selected.toml");
    std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n").expect("write explicit config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        Some(config_path),
        "the daemon spawn seam must receive the same explicit config path used to resolve the exec frame"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn memory_db_override_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

    let dir = tempfile::tempdir().expect("config tempdir");
    let config_path = dir.path().join("selected.toml");
    std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n").expect("write explicit config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(":memory:".to_string()),
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        Some(":memory:".to_string()),
        "the daemon spawn seam must receive the raw --db override so a spawned daemon \
             can be constructed with the same ephemeral in-memory storage"
    );
}

/// A declared read-only SQLite `main` backend becomes a writable memory
/// backend when `--db :memory:` forces the whole topology ephemeral. The
/// pre-open exec frame must fingerprint that effective runtime mode, not
/// the superseded declaration, or the freshly spawned daemon rejects its
/// very first request as a config mismatch.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn force_memory_exec_frame_matches_opened_read_only_topology_runtime() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    let (prev_home, _home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().expect("force-memory config tempdir");
    let config_path = fixture.path().join("read-only-topology.toml");
    let declared_main = fixture.path().join("declared-main.db");
    let declared_archive = fixture.path().join("declared-archive.db");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"
read_only = true

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
"#,
            declared_main.display(),
            declared_archive.display(),
        ),
    )
    .expect("write read-only topology config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&config_path),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve force-memory exec config");
    assert_eq!(
        cfg.db_path, None,
        "the force-memory anchor must be in-memory"
    );

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(":memory:".to_string()),
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(result.is_ok(), "force-memory dispatch failed: {result:?}");

    let frame_config_id = SPY_CAPTURED_CONFIG_ID
        .with(|captured| captured.borrow_mut().take())
        .expect("spy must capture the forwarded config id");
    let khive_cfg = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
        .expect("load force-memory topology")
        .expect("explicit config must exist");
    let opened =
        khive_mcp::serve::build_registry_for_multi_backend(cfg, &khive_cfg, Some(":memory:"))
            .await
            .expect("force-memory runtime must build");
    restore_home(prev_home);

    assert!(
        !opened.default_runtime.is_read_only(),
        "force-memory replaces the declared read-only SQLite main with writable memory"
    );
    assert_eq!(
        frame_config_id, opened.config_id,
        "the pre-open exec frame and opened force-memory runtime must have identical config ids"
    );
    assert!(
        !declared_main.exists() && !declared_archive.exists(),
        "force-memory parity setup must not materialize either declared SQLite path"
    );
}

#[cfg(unix)]
fn write_writable_multi_backend_config(
    config_path: &Path,
    main_path: &Path,
    secondary_path: &Path,
) {
    std::fs::write(
        config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
"#,
            main_path.display(),
            secondary_path.display(),
        ),
    )
    .unwrap();
}

#[cfg(unix)]
fn chmod_read_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(path, permissions).unwrap();

    // A writable fixture's connections can close asynchronously and leave
    // `-wal`/`-shm` sidecars behind; read-only admission rejects a writable
    // `-shm` as potentially live. Freeze any lingering sidecars so the
    // snapshot takes the documented frozen form.
    for suffix in ["-wal", "-shm"] {
        let mut name = path.file_name().unwrap().to_os_string();
        name.push(suffix);
        let sidecar = path.parent().unwrap().join(name);
        if sidecar.exists() {
            let mut sidecar_permissions = std::fs::metadata(&sidecar).unwrap().permissions();
            sidecar_permissions.set_mode(0o444);
            std::fs::set_permissions(&sidecar, sidecar_permissions).unwrap();
        }
    }
}

#[cfg(unix)]
fn runtime_config_for_explicit_multi_backend(
    config_path: &Path,
    db: Option<&str>,
) -> RuntimeConfig {
    resolve_runtime_config(RuntimeConfigInputs {
        db,
        config: Some(config_path),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .unwrap()
}

/// A client must not reuse a daemon that retained a write-capable handle
/// after the declared main file was chmod'd into snapshot mode. The
/// filesystem-mode refusal belongs before the forwarding seam.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn multi_backend_main_chmod_refuses_before_daemon_forward() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("khive.toml");
    let main_path = fixture.path().join("main.db");
    let archive_path = fixture.path().join("archive.db");
    std::fs::write(&main_path, b"main snapshot fixture").unwrap();
    std::fs::write(&archive_path, b"archive fixture").unwrap();
    chmod_read_only(&main_path);
    write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        runtime_config_for_explicit_multi_backend(&config_path, None),
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    let error = result.expect_err("an undeclared main snapshot mode must fail closed");
    assert!(error.to_string().contains("read_only = true"), "{error}");
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_none()),
        "the retained writable daemon must never receive the frame"
    );
}

/// The topology fingerprint includes secondary modes too; apply the same
/// pre-forward refusal to every declared SQLite backend, not only `main`.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn multi_backend_secondary_chmod_refuses_before_daemon_forward() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("khive.toml");
    let main_path = fixture.path().join("main.db");
    let archive_path = fixture.path().join("archive.db");
    std::fs::write(&main_path, b"main fixture").unwrap();
    std::fs::write(&archive_path, b"archive snapshot fixture").unwrap();
    chmod_read_only(&archive_path);
    write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        runtime_config_for_explicit_multi_backend(&config_path, None),
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    let error = result.expect_err("an undeclared secondary snapshot mode must fail closed");
    assert!(error.to_string().contains("archive"), "{error}");
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_none()),
        "the retained writable daemon must never receive the frame"
    );
}

/// `--db :memory:` supersedes every declared file. Its pre-open/runtime
/// parity must therefore skip filesystem-mode checks on those unused paths.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn force_memory_skips_declared_chmod_preflight_and_forwards() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("khive.toml");
    let main_path = fixture.path().join("main.db");
    let archive_path = fixture.path().join("archive.db");
    std::fs::write(&main_path, b"unused main snapshot fixture").unwrap();
    std::fs::write(&archive_path, b"unused archive snapshot fixture").unwrap();
    chmod_read_only(&main_path);
    chmod_read_only(&archive_path);
    write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        runtime_config_for_explicit_multi_backend(&config_path, Some(":memory:")),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(":memory:".to_string()),
            anchor: None,
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "force-memory forwarding must remain valid: {result:?}"
    );
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_some()),
        "the force-memory frame must reach the forwarding seam"
    );
}

/// A CONCRETE override on a single-backend invocation (no `[[backends]]`
/// declared) must reach the spawn seam: the spawned daemon has no
/// config-declared database path and would otherwise bind
/// `$HOME/.khive/khive.db`, never matching the client's override-anchored
/// frame. (The redundant multi-backend concrete case is withheld — see
/// `inline_db_override_guard_normalizes_main_config_id_and_rejects_conflict`.)
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn single_backend_concrete_db_override_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

    let dir = tempfile::tempdir().expect("config tempdir");
    // No [[backends]] declared — the single-backend shape.
    let config_path = dir.path().join("selected.toml");
    std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n").expect("write explicit config");

    let override_path = dir.path().join("override.db");
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(override_path.to_str().expect("utf8")),
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(override_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(override_path.to_str()),
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        Some(override_path.display().to_string()),
        "the single-backend concrete override must reach the daemon spawn seam so a \
             spawned daemon binds the operator's file instead of the default database"
    );
}

/// khive-oss#1941: a client whose environment sets `KHIVE_PACKS` must
/// forward that resolved pack list to any daemon it spawns, or the fresh
/// daemon defaults to the built-in pack set, its `config_id` disagrees
/// with every caller expecting the wider selection, and those callers
/// permanently fall back to in-process dispatch instead of the warm
/// daemon.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn env_khive_packs_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = EnvAndCwdGuard::capture();
    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::set_var("KHIVE_PACKS", "kg,gtd,memory");
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = None);

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve exec-shaped config from KHIVE_PACKS env");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_PACKS.with(|captured| captured.borrow_mut().take()),
        Some(vec![
            "kg".to_string(),
            "gtd".to_string(),
            "memory".to_string()
        ]),
        "the KHIVE_PACKS-resolved pack list must reach the daemon spawn seam so a \
             spawned daemon serves the same packs this client resolved"
    );
}

/// Control for `env_khive_packs_reaches_daemon_spawn_seam`: with no
/// `KHIVE_PACKS` (or other pack-selection input) set, the client resolves
/// the built-in production pack set and forwards exactly that — a
/// spawned daemon re-deriving the same built-in default independently
/// would reach the identical outcome, so this proves the fix is additive
/// and does not change the unconfigured default path's effective result.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn no_env_control_forwards_built_in_default_packs_to_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    // Declared first so it drops LAST (reverse declaration order):
    // constructing the guard here, before either tempdir is created and
    // before any process-global mutation, means every panic from this
    // point on — including a `tempdir()`/`set_current_dir()` setup panic,
    // not just a later assertion — has a live guard whose Drop restores
    // KHIVE_PACKS/HOME/cwd. Because it drops last, `home_dir` and
    // `empty_project_root` are removed BEFORE that restore runs, i.e.
    // `empty_project_root` is removed while it is still the process cwd
    // (verified empirically below: `TempDir::drop` tolerates removing the
    // current working directory on macOS and ignores its own errors).
    let _guard = EnvAndCwdGuard::capture();

    // Isolate both HOME and cwd: with `db: None` config discovery falls
    // through to tier-2 (`<cwd>/khive.toml`) then tier-4
    // (`~/.khive/config.toml`) — either one declaring `[runtime].packs`
    // would satisfy this control incidentally instead of proving the
    // built-in-default path, so neither may be the ambient machine's.
    // Creating these tempdirs mutates nothing process-global by itself
    // (a panic here would be caught by the guard above, harmlessly
    // restoring values that were never changed).
    let home_dir = tempfile::tempdir().expect("tempdir for isolated HOME");
    let empty_project_root = tempfile::tempdir().expect("empty project-root tempdir");

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::set_var("HOME", home_dir.path());
    std::env::set_current_dir(empty_project_root.path())
        .expect("chdir into isolated project root with no discoverable config");

    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = None);

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve exec-shaped config with no pack-selection input");

    // Isolating HOME/cwd only keeps the ambient machine's config from
    // leaking in; it does not by itself prove resolution landed on the
    // built-in set rather than some other value. Certify that directly
    // before the forwarding seam is exercised.
    assert_eq!(
        cfg.packs,
        RuntimeConfig::built_in_packs(),
        "the isolated no-selection environment must resolve to the built-in default \
             pack set before the forwarding seam is exercised"
    );

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_PACKS.with(|captured| captured.borrow_mut().take()),
        Some(RuntimeConfig::built_in_packs()),
        "with no pack-selection input the client must forward exactly the built-in \
             default pack set — identical to what an independently-spawned daemon would \
             have defaulted to on its own"
    );
}

/// The redundant-multi-backend spawn decision (override withheld from the
/// spawned daemon) has a config-side twin: when no explicit `--config`
/// was given, the config that declared the backend topology was
/// DISCOVERED (here via the db-dir tier-3 anchor of
/// `KhiveConfig::load_with_home_fallback_and_source`), and the withheld
/// override was the child's only other clue about which database to
/// bind. The spawn seam must receive that retained resolved path as the
/// child's explicit `--config`, or the spawned daemon re-discovers from
/// its own cwd/HOME, cannot reach a config anchored only beside the
/// database, binds `$HOME/.khive/khive.db`, and squats the socket with a
/// `config_id` that never matches the normalized frame.
///
/// Control arms: with an explicit config the seam receives the explicit
/// path (never the discovered one), and in the empty-backends case the
/// seam receives no config at all (the concrete override supplies the
/// database directly).
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn redundant_db_override_forwards_discovered_config_to_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    let (prev_home, _home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

    // A multi-backend config discoverable ONLY via the db-dir tier-3
    // anchor (`project_config_anchor_dir`): it lives in
    // `<main-db-dir>/.khive/config.toml`, not at the process cwd and not
    // under `$HOME/.khive`.
    let backend_dir = tempfile::tempdir().expect("backend tempdir");
    let main_backend_path = backend_dir.path().join("main-backend.db");
    let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
    let anchor_dir = backend_dir.path().join(".khive");
    std::fs::create_dir_all(&anchor_dir).expect("mkdir db-dir anchor");
    let discovered_config_path = anchor_dir.join("config.toml");
    std::fs::write(
        &discovered_config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write tier-3 multi-backend config");
    let canonical_config_path =
        std::fs::canonicalize(&discovered_config_path).expect("canonicalize config path");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(main_backend_path.to_str().expect("utf8")),
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    // ── the fix case: redundant multi-backend, no explicit config ──
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(main_backend_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(main_backend_path.to_str()),
            config: None,
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(
        result.is_ok(),
        "redundant-override dispatch must reach daemon forwarding: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        None,
        "the redundant override stays withheld from the spawn seam"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        Some(canonical_config_path.clone()),
        "the spawn seam must receive the retained resolved config path as the \
             child's explicit --config when the redundant override is withheld"
    );

    // ── control: explicit config wins, discovered path is not substituted ──
    let explicit_config_path = backend_dir.path().join("explicit.toml");
    std::fs::copy(&discovered_config_path, &explicit_config_path)
        .expect("copy topology as explicit config");
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(main_backend_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(main_backend_path.to_str()),
            config: Some(explicit_config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(
        result.is_ok(),
        "explicit-config dispatch must reach daemon forwarding: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        Some(explicit_config_path),
        "with an explicit config the seam receives the operator's path, never a discovered one"
    );

    // ── control: empty backends get no config, only the concrete override ──
    let single_dir = tempfile::tempdir().expect("single-backend tempdir");
    let override_path = single_dir.path().join("override.db");
    let single_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(override_path.to_str().expect("utf8")),
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve single-backend exec-shaped config");
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        single_cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(override_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(override_path.to_str()),
            config: None,
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(
        result.is_ok(),
        "single-backend dispatch must reach daemon forwarding: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        None,
        "the empty-backends case forwards no config — the concrete override supplies the database"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        Some(override_path.display().to_string()),
        "the single-backend concrete override still reaches the spawn seam"
    );

    restore_home(prev_home);
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn explicit_config_is_loaded_for_exec_forward_frame() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    let (prev_home, _home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().expect("config fixture tempdir");
    let config_path = fixture.path().join("code-map.toml");
    let main_backend_path = fixture.path().join("code-map.db");
    let sessions_backend_path = fixture.path().join("sessions.db");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write explicit exec config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("namespace"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve explicit exec config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(
        result.is_ok(),
        "explicit-config dispatch failed: {result:?}"
    );

    let captured = SPY_CAPTURED_CONFIG_ID
        .with(|value| value.borrow_mut().take())
        .expect("spy must capture the forwarded config id");
    let khive_cfg = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
        .expect("load explicit config")
        .expect("explicit config must exist");
    let mut expected_config = cfg;
    expected_config.db_path = Some(main_backend_path);
    let expected = compute_config_id(&expected_config, Some(&khive_cfg));
    restore_home(prev_home);

    assert_eq!(
        captured, expected,
        "the exec forward frame must fold the explicitly selected backend topology"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn exec_frame_config_id_matches_daemon_config_id_for_multi_backend_project_toml() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    let (prev_home, home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

    // No explicit `--db` anywhere below — this mirrors the real multi-tenant
    // deployment shape the bug affects: `~/.khive/config.toml` declares
    // `[[backends]]` and `kkernel exec` relies on default discovery.
    // A divergent explicit `--db` would be rejected as ambiguous once
    // backends are declared; repeating the main path would be accepted but
    // would not model this default-discovery scenario.
    let khive_dir = home_dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    // Keep the configuration home-shaped while placing the stores in a
    // separate tempdir. Test-harness builds reject every store under
    // `$HOME/.khive`, including isolated fixtures, at the open boundary.
    let backend_dir = tempfile::tempdir().expect("backend tempdir");
    let main_backend_path = backend_dir.path().join("main-backend.db");
    let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
    std::fs::write(
        khive_dir.join("config.toml"),
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"

[packs.session]
backend = "sessions"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write multi-backend config.toml");

    // `no_embed: true` keeps this test fast and network-independent — it is
    // scoped to the backends-topology fold, not embedding-model resolution
    // (a separate, already-covered concern in the sibling project-toml test).
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (#1276) — this test's assertion is
        // about config_id parity, not about pack resolution.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(result.is_ok(), "exec dispatch must succeed: {result:?}");

    let captured = SPY_CAPTURED_CONFIG_ID
        .with(|c| c.borrow_mut().take())
        .expect("spy must have captured a forwarded frame");

    // Independently compute what the DAEMON would compute for the exact
    // same on-disk config.toml + database, mirroring serve.rs's own boot
    // path (`build_server`): resolve_runtime_config with
    // namespace_explicit=false (the daemon-startup shape), load the same
    // KhiveConfig, and fold it with Some(&khive_cfg) exactly like
    // serve.rs:916 does.
    let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: true,
        // Same pin as `cfg` above (#1276) — both sides of the parity
        // comparison must resolve identically regardless of ambient
        // `KHIVE_PACKS`.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve serve-shaped config");
    let khive_cfg = KhiveConfig::load_with_home_fallback(None, serve_cfg.db_path.as_deref())
        .expect("load multi-backend config.toml")
        .expect("config.toml must be found at tier 3");
    assert!(
        !khive_cfg.backends.is_empty(),
        "sanity: the written config.toml must actually resolve with a non-empty \
             backends list, or this test proves nothing"
    );
    let mut declared_main_config = serve_cfg;
    declared_main_config.db_path = Some(main_backend_path);
    let daemon_config_id = compute_config_id(&declared_main_config, Some(&khive_cfg));
    restore_home(prev_home);

    assert_eq!(
        captured, daemon_config_id,
        "the config_id in the ACTUAL frame run_exec_inline_with_forward sends to the \
             daemon must be byte-identical to what the daemon computes for the same \
             multi-backend config.toml (D1 acceptance gate, exercised end-to-end through \
             the real call site rather than a standalone compute_config_id comparison)"
    );
}

/// With no --db override, a declared memory main replaces the unused
/// HOME-shaped anchor before forwarding. If no daemon answers, local
/// construction must receive that normalized anchor too.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn declared_topology_exec_fallback_updates_omitted_db_anchor() {
    if crate::test_process::run_in_child() {
        return;
    }

    let fixture = tempfile::tempdir().expect("config fixture");
    let unused_home = fixture.path().join("unused-home");
    let anchor = unused_home.join(".khive/khive.db");
    let config_path = fixture.path().join("memory-topology.toml");
    std::fs::write(
        &config_path,
        "[[backends]]\nname = \"main\"\nkind = \"memory\"\n",
    )
    .expect("write explicit memory topology");
    let cfg = RuntimeConfig {
        db_path: Some(anchor.clone()),
        actor_id: Some("config-anchor-fallback-test".to_string()),
        packs: vec!["kg".to_string()],
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    };
    let topology = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
        .unwrap()
        .expect("explicit topology exists");
    let mut expected_cfg = cfg.clone();
    expected_cfg.db_path = None;
    let expected_id = compute_config_id(&expected_cfg, Some(&topology));
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        Some("json".to_string()),
        None,
        ExecDbContext {
            raw: None,
            anchor: Some(anchor),
            config: Some(config_path),
        },
        true,
        spy_capture_config_id,
    )
    .await;

    assert!(result.is_ok(), "local fallback must dispatch: {result:?}");
    assert_eq!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow_mut().take()),
        Some(expected_id),
        "the forward attempt must use declared memory before falling back"
    );
    assert!(
        !unused_home.exists(),
        "the superseded HOME-shaped database anchor must never be opened"
    );
}

// ── #1226: inline --db/[[backends]] guard must fire before daemon-forward ──

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn inline_db_override_guard_normalizes_main_config_id_and_rejects_conflict() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    let (prev_home, home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

    let khive_dir = home_dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    // Keep the configuration home-shaped while placing the stores in a
    // separate tempdir. Test-harness builds reject every store under
    // `$HOME/.khive`, including isolated fixtures, at the open boundary.
    let backend_dir = tempfile::tempdir().expect("backend tempdir");
    let main_backend_path = backend_dir.path().join("main-backend.db");
    let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
    std::fs::write(
        khive_dir.join("config.toml"),
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"

[packs.session]
backend = "sessions"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write multi-backend config.toml");

    let no_override_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        // Pin the pack list rather than inheriting `KHIVE_PACKS` from the
        // ambient environment (#1276) — an ambient list naming packs not
        // compiled into this build would fail resolution before the
        // behavior under test.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config without override");
    let no_override_result = run_exec_inline_with_forward(
        "stats()".to_string(),
        no_override_cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(
        no_override_result.is_ok(),
        "no-override dispatch must succeed: {no_override_result:?}"
    );
    let no_override_config_id = SPY_CAPTURED_CONFIG_ID
        .with(|captured| captured.borrow_mut().take())
        .expect("no-override frame must be captured");

    let matching_override = main_backend_path.display().to_string();
    // Sentinel: proves a captured None below means "the seam was called
    // with None" (withheld override), not "the spy was never invoked".
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = Some("sentinel".to_string()));
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&matching_override),
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        // Pin the pack list rather than inheriting `KHIVE_PACKS` from the
        // ambient environment (#1276) — an ambient list naming packs not
        // compiled into this build would fail resolution before the
        // behavior under test.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let matching_result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(matching_override.clone()),
            anchor: khive_runtime::resolve_db_anchor(Some(&matching_override)),
            config: None,
        },
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(
        matching_result.is_ok(),
        "an override matching the declared main backend must reach daemon forwarding: {matching_result:?}"
    );
    let matching_config_id = SPY_CAPTURED_CONFIG_ID
        .with(|captured| captured.borrow_mut().take())
        .expect("matching-override frame must be captured");
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        None,
        "the redundant multi-backend concrete override must be WITHHELD from the spawn \
             seam: the frame's fingerprint is normalized to the no-override anchor, and the \
             spawned daemon's config-declared main path IS the override's target"
    );

    let conflicting_override = backend_dir.path().join("override.db");
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(conflicting_override.display().to_string()),
            anchor: None,
            config: None,
        },
        false,
        spy_capture_config_id,
    )
    .await;
    restore_home(prev_home);

    assert!(
        result.is_err(),
        "a --db/KHIVE_DB override that conflicts with a declared [[backends]] topology \
             must be rejected on the inline path too, not only on --ops-file; got: {result:?}"
    );
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|c| c.borrow().is_none()),
        "the conflict must be caught BEFORE any daemon-forward attempt — the spy must \
             never have been called"
    );
    assert_eq!(
        matching_config_id, no_override_config_id,
        "a matching --db override must emit the same config_id as no override for the same multi-backend config"
    );
}

#[tokio::test]
async fn ops_file_malformed_line_aborts_before_writes() {
    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    // Line 1: valid op
    f.write_all(
        b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"ShouldNotExist\"}}\n",
    )
    .unwrap();
    // Line 2: malformed
    f.write_all(b"INVALID JSON LINE\n").unwrap();

    let path = f.path().to_path_buf();

    // parse_ops_file should fail with line 2 error.
    let err = parse_ops_file(&path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("line 2"),
        "should report line 2 as malformed: {msg}"
    );

    // Because parse failed, no dispatch happened → DB is clean.
    let server = isolated_server(&db_path);
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let count = resp["results"][0]["result"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "nothing should be written when any line fails to parse"
    );
}

// ── ADR-099 B3: `--atomic` CLI surface acceptance tests ───────────────────

fn atomic_op(tool: &str, args: serde_json::Value) -> OpsFileEntry {
    OpsFileEntry {
        tool: tool.to_string(),
        args,
    }
}

async fn dispatch_json(server: &KhiveMcpServer, ops: &str) -> serde_json::Value {
    // Verbose presentation: the default Agent mode truncates entity ids
    // to an 8-char short form for readability, which the atomic prepare
    // path (and every KG verb) rejects as "not a full UUID". Tests here
    // need the real id back out so it can feed straight into `update`/
    // `delete`/`link` args.
    let params = RequestParams {
        plan: None,
        ops: ops.to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn atomic_cfg(db_path: &str) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(PathBuf::from(db_path)),
        embedding_model: None,
        additional_embedding_models: vec![],
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (#1276). Atomic execution retains
        // the complete discovered validation/lifecycle surface even when
        // the caller configures only the base KG pack.
        packs: vec!["kg".to_string()],
        ..Default::default()
    }
}

async fn replace_fts_entities_with_incompatible_table(db_path: &str) {
    let runtime = KhiveRuntime::new(atomic_cfg(db_path)).expect("runtime for FTS fault setup");
    let sql = runtime.sql();
    let mut writer = sql.writer().await.expect("writer for FTS fault setup");
    writer
        .execute_script(
            "DROP TABLE fts_entities; \
                 CREATE TABLE fts_entities (broken_column TEXT);"
                .to_string(),
        )
        .await
        .expect("replace FTS table to inject post-commit reindex failure");
}

#[tokio::test]
async fn atomic_kg_only_config_keeps_gtd_hook_and_lifecycle_execution() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let (hook_task_id, transition_task_id, complete_task_id) = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"[gtd.assign(title="HookGuard", status="next"), gtd.assign(title="TransitionGuard", status="inbox"), gtd.assign(title="CompleteGuard", status="active")]"#,
        )
        .await;
        let full_id = |index: usize| {
            response["results"][index]["result"]["full_id"]
                .as_str()
                .unwrap_or_else(|| panic!("missing task full_id at index {index}: {response}"))
                .to_string()
        };
        (full_id(0), full_id(1), full_id(2))
    };

    let hook_error = crate::atomic_apply::execute_atomic_ops_file(
        vec![atomic_op(
            "update",
            serde_json::json!({
                "id": hook_task_id.as_str(),
                "properties": {"depends_on": [hook_task_id.as_str()]},
            }),
        )],
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("the GTD task hook must reject a self-dependency");
    assert!(
        format!("{hook_error:#}").contains("cannot depend on itself"),
        "the kg-only atomic registry must enforce the GTD hook: {hook_error:#}"
    );

    let server = isolated_server(&db_path);
    let response = dispatch_json(&server, &format!(r#"get(id="{hook_task_id}")"#)).await;
    assert!(
        response["results"][0]["result"]["properties"]
            .get("depends_on")
            .is_none(),
        "the rejected dependency update must not mutate the task: {response}"
    );

    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "gtd.transition",
                serde_json::json!({"id": transition_task_id, "status": "next"}),
            ),
            atomic_op(
                "gtd.complete",
                serde_json::json!({"id": complete_task_id, "result": "verified"}),
            ),
        ],
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("GTD lifecycle adapters must execute with a kg-only config");
    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(envelope["results"][0]["result"]["to"], "next");
    assert_eq!(envelope["results"][1]["result"]["to"], "done");
}

/// Acceptance test 1a: an all-success atomic ops-file run commits every
/// op as one unit and the results are visible afterward.
#[tokio::test]
async fn atomic_ops_file_success_commits_all_ops() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (x_id, y_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="AtomicX"), create(kind="concept", name="AtomicY")]"#,
        )
        .await;
        let x_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        let y_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("y id")
            .to_string();
        (x_id, y_id)
    };

    let ops = vec![
        atomic_op(
            "update",
            serde_json::json!({"id": x_id, "name": "AtomicX-renamed"}),
        ),
        atomic_op(
            "update",
            serde_json::json!({"id": y_id, "name": "AtomicY-renamed"}),
        ),
    ];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic run must succeed");

    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let x_resp = dispatch_json(&server, &format!(r#"get(id="{x_id}")"#)).await;
    let y_resp = dispatch_json(&server, &format!(r#"get(id="{y_id}")"#)).await;
    assert_eq!(x_resp["results"][0]["result"]["name"], "AtomicX-renamed");
    assert_eq!(y_resp["results"][0]["result"]["name"], "AtomicY-renamed");
}

#[tokio::test]
async fn atomic_post_commit_reindex_failure_returns_committed_non_retryable_envelope() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let entity_id = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"create(kind="concept", name="PostCommitReindexBefore")"#,
        )
        .await;
        response["results"][0]["result"]["id"]
            .as_str()
            .expect("entity id")
            .to_string()
    };

    // Migration state remains current, but an incompatible ordinary table
    // occupies the post-commit FTS name. Store initialization cannot
    // recreate the virtual table through IF NOT EXISTS; prepare and base
    // DML do not touch it, so the failure occurs at the real reindex seam.
    replace_fts_entities_with_incompatible_table(&db_path).await;

    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![atomic_op(
            "update",
            serde_json::json!({"id": entity_id, "name": "PostCommitReindexAfter"}),
        )],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("post-commit failure must return a reconciliation envelope");

    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(
        envelope["atomic"]["status"], "committed_degraded",
        "{envelope}"
    );
    assert_eq!(envelope["atomic"]["retryable"], false, "{envelope}");
    assert_eq!(
        envelope["atomic"]["degradations"][0]["stage"], "post_commit_reindex",
        "{envelope}"
    );
    assert_eq!(envelope["summary"]["succeeded"], 1, "{envelope}");

    let runtime = KhiveRuntime::new(atomic_cfg(&db_path)).expect("runtime for committed-row check");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let entity = runtime
        .get_entity(&token, Uuid::parse_str(&entity_id).unwrap())
        .await
        .expect("committed entity row");
    assert_eq!(entity.name, "PostCommitReindexAfter");
}

#[tokio::test]
async fn atomic_result_read_failure_keeps_later_committed_delete_non_retryable() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let entity_id = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"create(kind="concept", name="RenderThenDelete")"#,
        )
        .await;
        response["results"][0]["result"]["id"]
            .as_str()
            .expect("entity id")
            .to_string()
    };

    // Both plans prepare against the same live row. The unit then updates
    // and hard-deletes it atomically. Rendering op 0 performs its real
    // post-commit read and cannot find the row removed by op 1.
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "update",
                serde_json::json!({"id": entity_id, "name": "NeverRendered"}),
            ),
            atomic_op("delete", serde_json::json!({"id": entity_id, "hard": true})),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("render failure after commit must be a reconciliation envelope");

    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(
        envelope["atomic"]["status"], "committed_degraded",
        "{envelope}"
    );
    assert_eq!(envelope["atomic"]["retryable"], false, "{envelope}");
    assert_eq!(
        envelope["atomic"]["degradations"][0]["stage"], "result_rendering",
        "{envelope}"
    );
    assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
    assert_eq!(
        envelope["results"][0]["status"], "committed_degraded",
        "{envelope}"
    );
    assert_eq!(envelope["results"][0]["retryable"], false, "{envelope}");
    assert_eq!(envelope["results"][1]["result"]["deleted"], true);

    let runtime = KhiveRuntime::new(atomic_cfg(&db_path)).expect("runtime for deleted-row check");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let deleted = runtime
        .get_entity(&token, Uuid::parse_str(&entity_id).unwrap())
        .await;
    assert!(deleted.is_err(), "hard delete must remain committed");
}

/// Acceptance test 1b: a mid-unit failure rolls the WHOLE unit back —
/// zero partial state, including the op that "succeeded" before the
/// failing one.
///
/// Shape: `x` and `y` both exist. Op 0 hard-deletes `x`. Op 1 links `y`
/// to `x`. At PREPARE time (before either op runs) `x` still exists, so
/// both plans build successfully. At COMMIT time op 0 removes `x` first,
/// then op 1's guarded `INSERT ... WHERE EXISTS` affects zero rows (the
/// dangling-edge guard, ADR-099 D1 rule 1) — the whole unit rolls back,
/// so `x`'s deletion is undone too.
#[tokio::test]
async fn atomic_ops_file_mid_unit_failure_rolls_back_whole_unit() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (x_id, y_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="RollbackX"), create(kind="concept", name="RollbackY")]"#,
        )
        .await;
        let x_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        let y_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("y id")
            .to_string();
        (x_id, y_id)
    };

    let ops = vec![
        atomic_op("delete", serde_json::json!({"id": x_id, "hard": true})),
        atomic_op(
            "link",
            serde_json::json!({
                "source_id": y_id,
                "target_id": x_id,
                "relation": "extends",
            }),
        ),
    ];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("the seam call itself must not error — the unit rolls back cleanly");

    assert_eq!(
        envelope["atomic"]["rolled_back"], true,
        "envelope: {envelope}"
    );
    assert_eq!(
        envelope["atomic"]["failed_op_index"], 1,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let x_resp = dispatch_json(&server, &format!(r#"get(id="{x_id}")"#)).await;
    assert!(
        x_resp["results"][0]["result"]["deleted_at"].is_null(),
        "x must NOT be deleted — the whole unit must have rolled back: {x_resp}"
    );
}

#[tokio::test]
async fn atomic_rollback_exits_nonzero_with_or_without_save_and_strict() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let (x_id, y_id) = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"[create(kind="concept", name="AtomicExitX"), create(kind="concept", name="AtomicExitY")]"#,
        )
        .await;
        (
            response["results"][0]["result"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
            response["results"][1]["result"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let mut file = NamedTempFile::new().unwrap();
    for op in [
        serde_json::json!({"tool":"delete","args":{"id":x_id.clone(),"hard":true}}),
        serde_json::json!({
            "tool":"link",
            "args":{"source_id":y_id,"target_id":x_id,"relation":"extends"}
        }),
    ] {
        serde_json::to_writer(&mut file, &op).unwrap();
        file.write_all(b"\n").unwrap();
    }
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").unwrap();
    let db_context = || ExecDbContext {
        raw: Some(db_path.clone()),
        anchor: Some(PathBuf::from(&db_path)),
        config: Some(config_path.clone()),
    };

    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        None,
        None,
        false,
        db_context(),
        false,
        true,
        None,
        false,
    )
    .await
    .expect_err("atomic rollback must exit non-zero without --strict");
    assert!(error.to_string().contains("rolled back"), "{error:#}");

    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("atomic-rollback.jsonl");
    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        false,
        db_context(),
        false,
        true,
        None,
        true,
    )
    .await
    .expect_err("atomic rollback must exit non-zero with --strict and --save-file");
    assert!(error.to_string().contains("rolled back"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(save_path).unwrap().lines().count(),
        2
    );
}

#[test]
fn atomic_save_persist_failure_returns_committed_reconciliation_stdout() {
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("publish-race.jsonl");
    let sink =
        khive_mcp::save_sink::JsonlSaveSink::new(&save_path, false).expect("preflight save sink");
    // Deterministic post-preflight publication failure: a directory wins
    // the destination path after the sibling temp file has been created,
    // so row writes and flush succeed but the final atomic rename fails.
    std::fs::create_dir(&save_path).expect("occupy destination with a directory");
    let mut envelope = serde_json::json!({
        "results": [{
            "ok": true,
            "tool": "update",
            "op_index": 0,
            "result": {"id": Uuid::new_v4()}
        }],
        "summary": {"total": 1, "succeeded": 1, "failed": 0},
        "atomic": {
            "committed": true,
            "rolled_back": false,
            "failed_op_index": null,
            "error": null
        }
    });

    let failure = render_atomic_output(&mut envelope, Some(sink))
        .expect_err("persist failure must remain a non-zero CLI outcome");
    let stdout: serde_json::Value =
        serde_json::from_str(&failure.stdout).expect("structured stdout envelope");

    assert_eq!(stdout["atomic"]["committed"], true, "{stdout}");
    assert_eq!(stdout["atomic"]["status"], "committed_degraded", "{stdout}");
    assert_eq!(stdout["atomic"]["retryable"], false, "{stdout}");
    assert_eq!(
        stdout["atomic"]["degradations"][0]["stage"], "save_file_publish",
        "{stdout}"
    );
    assert!(
        stdout["atomic"]["degradations"][0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("persist temp file")),
        "{stdout}"
    );
    assert!(
        format!("{:#}", failure.error).contains("do not replay the mutation"),
        "terminal error must point automation at reconciliation"
    );
}

#[tokio::test]
async fn atomic_invalid_save_directory_is_rejected_before_commit() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let mut file = NamedTempFile::new().unwrap();
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "tool":"create",
            "args":{"kind":"concept","name":"atomic-must-not-exist"}
        }),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").unwrap();
    let save_directory = tempfile::tempdir().unwrap();
    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        Some("json".to_string()),
        Some(save_directory.path().to_string_lossy().into_owned()),
        false,
        ExecDbContext {
            raw: Some(db_path.clone()),
            anchor: Some(PathBuf::from(&db_path)),
            config: Some(config_path),
        },
        false,
        true,
        None,
        true,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("absent or an existing regular file"));

    let server = isolated_server(&db_path);
    let response = dispatch_json(&server, r#"list(kind="concept")"#).await;
    assert_eq!(
        response["results"][0]["result"]["items"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn atomic_preflighted_save_keeps_prior_file_on_execution_error() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let mut file = NamedTempFile::new().unwrap();
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "tool":"create",
            "args":{"kind":"concept","name":"atomic-rejected"}
        }),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("prior.jsonl");
    std::fs::write(&save_path, b"prior-complete-output\n").unwrap();

    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        false,
        ExecDbContext {
            raw: Some(db_path),
            anchor: Some(PathBuf::from(db_file.path())),
            config: Some(config_path),
        },
        false,
        true,
        Some(1),
        true,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("--atomic rejected"),
        "unexpected atomic execution error: {error:#}"
    );
    assert_eq!(
        std::fs::read(&save_path).unwrap(),
        b"prior-complete-output\n"
    );
}

/// #1474: the user-facing `--atomic` executor prepares every operation
/// before its commit pass. Two individually acyclic task writes can
/// therefore form a cycle only inside the unit. The V16 commit-time
/// guards must reject the later statement and roll the earlier one back
/// for both authoritative dependency stores.
#[tokio::test]
async fn atomic_ops_file_rejects_same_unit_gtd_dependency_cycles() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (a_id, b_id) = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"[gtd.assign(title="AtomicCycleA", status="next"), gtd.assign(title="AtomicCycleB", status="next")]"#,
        )
        .await;
        (
            response["results"][0]["result"]["full_id"]
                .as_str()
                .expect("task A id")
                .to_string(),
            response["results"][1]["result"]["full_id"]
                .as_str()
                .expect("task B id")
                .to_string(),
        )
    };

    let compact_a_id = a_id.replace('-', "");
    let compact_b_id = b_id.replace('-', "");
    let alternate_spelling_error = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "update",
                serde_json::json!({
                    "id": a_id.clone(),
                    "properties": {"depends_on": [compact_b_id]}
                }),
            ),
            atomic_op(
                "update",
                serde_json::json!({
                    "id": b_id.clone(),
                    "properties": {"depends_on": [compact_a_id]}
                }),
            ),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("atomic preparation must reject an alternate dependency UUID spelling");
    let alternate_spelling_message = format!("{alternate_spelling_error:#}");
    assert!(
        alternate_spelling_message.contains("canonical lowercase hyphenated UUID"),
        "unexpected alternate-spelling error: {alternate_spelling_message}"
    );

    {
        let server = isolated_server(&db_path);
        let response = dispatch_json(&server, &format!(r#"get(id="{a_id}")"#)).await;
        assert!(
            response["results"][0]["result"]["properties"]
                .get("depends_on")
                .is_none(),
            "alternate dependency spelling must not persist: {response}"
        );
    }

    let property_envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "update",
                serde_json::json!({
                    "id": a_id.clone(),
                    "properties": {"depends_on": [b_id.clone()]}
                }),
            ),
            atomic_op(
                "update",
                serde_json::json!({
                    "id": b_id.clone(),
                    "properties": {"depends_on": [a_id.clone()]}
                }),
            ),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("cycle is a clean atomic rollback, not a seam failure");
    assert_eq!(property_envelope["atomic"]["rolled_back"], true);
    assert_eq!(property_envelope["atomic"]["failed_op_index"], 1);
    assert!(
        property_envelope["atomic"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("dependency cycle")),
        "envelope: {property_envelope}"
    );

    let server = isolated_server(&db_path);
    for task_id in [&a_id, &b_id] {
        let response = dispatch_json(&server, &format!(r#"get(id="{task_id}")"#)).await;
        assert!(
            response["results"][0]["result"]["properties"]
                .get("depends_on")
                .is_none(),
            "the earlier update must roll back too: {response}"
        );
    }

    let edge_envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": a_id.clone(),
                    "target_id": b_id.clone(),
                    "relation": "depends_on"
                }),
            ),
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": b_id.clone(),
                    "target_id": a_id.clone(),
                    "relation": "depends_on"
                }),
            ),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("edge cycle is a clean atomic rollback, not a seam failure");
    assert_eq!(edge_envelope["atomic"]["rolled_back"], true);
    assert_eq!(edge_envelope["atomic"]["failed_op_index"], 1);
    assert!(
        edge_envelope["atomic"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("dependency cycle")),
        "envelope: {edge_envelope}"
    );

    let server = isolated_server(&db_path);
    let response = dispatch_json(
        &server,
        &format!(r#"neighbors(id="{a_id}", direction="out", relations=["depends_on"])"#),
    )
    .await;
    assert_eq!(
        response["results"][0]["result"],
        serde_json::json!([]),
        "the earlier link must roll back too: {response}"
    );
}

/// ADR-099 B3 (second half): the inverse
/// same-unit race — `[link(A, B, competes_with), update(X
/// extends A-B -> competes_with)]`, where the CANONICAL row the update
/// conflict-absorbs into is created by an EARLIER op in the SAME
/// atomic unit (so it does not exist at either op's prepare time). The
/// commit must both write correctly (X deleted, the just-linked row
/// preserved unchanged per ADR-039 DO NOTHING — X's patch is discarded,
/// not applied) and RENDER the correct surviving id — not X's
/// prepare-time-advisory id, which this fix removed reliance on
/// entirely (`build_op_result` now derives it from a post-commit
/// natural-key lookup).
#[tokio::test]
async fn atomic_symmetric_update_absorbs_into_same_unit_link_and_renders_correct_id() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (a_id, b_id, x_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="LinkRaceA"), create(kind="concept", name="LinkRaceB")]"#,
        )
        .await;
        let a_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("a id")
            .to_string();
        let b_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("b id")
            .to_string();

        let link_resp = dispatch_json(
            &server,
            &format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.2)"#
            ),
        )
        .await;
        let x_id = link_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        (a_id, b_id, x_id)
    };

    let ops = vec![
        atomic_op(
            "link",
            serde_json::json!({
                "source_id": a_id,
                "target_id": b_id,
                "relation": "competes_with",
                "weight": 0.6,
            }),
        ),
        atomic_op(
            "update",
            serde_json::json!({"id": x_id, "relation": "competes_with", "weight": 0.9}),
        ),
    ];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic run must succeed");

    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let linked_id = envelope["results"][0]["result"]["id"]
        .as_str()
        .expect("link result id")
        .to_string();
    let rendered_update_id = envelope["results"][1]["result"]["id"]
        .as_str()
        .expect("update result id")
        .to_string();
    assert_ne!(
        rendered_update_id, x_id,
        "the update's rendered result must NOT be X's stale requested id: {envelope}"
    );
    assert_eq!(
        rendered_update_id, linked_id,
        "the update's rendered result must be the surviving (just-linked) row: {envelope}"
    );
    assert_eq!(
        envelope["results"][1]["result"]["weight"], 0.6,
        "ADR-039 DO NOTHING: the surviving row keeps its OWN pre-existing weight (0.6, \
             set by the link above), not the discarded update's patched weight (0.9): {envelope}"
    );

    let server = isolated_server(&db_path);
    let surviving_resp = dispatch_json(&server, &format!(r#"get(id="{linked_id}")"#)).await;
    assert_eq!(
        surviving_resp["results"][0]["result"]["weight"], 0.6,
        "the committed row itself must keep its pre-existing weight, not the discarded \
             update's patch: {surviving_resp}"
    );
}

/// The canonical survivor a
/// symmetric-update op absorbs into can ALREADY be soft-deleted before the
/// atomic unit even runs (not just tombstoned as a side effect of the same
/// unit's own writes, as the sibling test above covers). This exercises
/// `build_op_result`'s `get_edge_by_natural_key_including_deleted` call
/// through the real atomic path with a genuinely pre-existing tombstone: the
/// pre-fix renderer (`KhiveRuntime::list_edges`, which unconditionally
/// filters `deleted_at IS NULL`) would report the committed update as "not
/// found" for exactly this row, turning a successful DO NOTHING absorption
/// into a spurious post-commit error.
#[tokio::test]
async fn atomic_symmetric_update_absorbs_into_pre_existing_tombstoned_survivor_and_renders_it() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (canonical_id, x_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="TombA"), create(kind="concept", name="TombB")]"#,
        )
        .await;
        let a_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("a id")
            .to_string();
        let b_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("b id")
            .to_string();

        // The canonical survivor: created, then soft-deleted, BEFORE the atomic
        // unit ever runs.
        let link_resp = dispatch_json(
            &server,
            &format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="competes_with", weight=0.6)"#
            ),
        )
        .await;
        let canonical_id = link_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("canonical id")
            .to_string();
        dispatch_json(&server, &format!(r#"delete(id="{canonical_id}")"#)).await;

        // A distinct pre-existing edge under a different relation, later
        // converted (by the atomic update below) into the same
        // (a, b, competes_with) natural key — it must absorb into the
        // already-tombstoned canonical row, not resurrect or overwrite it.
        let x_resp = dispatch_json(
            &server,
            &format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.2)"#
            ),
        )
        .await;
        let x_id = x_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        (canonical_id, x_id)
    };

    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": x_id, "relation": "competes_with", "weight": 0.9}),
    )];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic run must succeed by absorbing into the tombstoned survivor");

    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let rendered_id = envelope["results"][0]["result"]["id"]
        .as_str()
        .expect("update result id")
        .to_string();
    assert_eq!(
        rendered_id, canonical_id,
        "must render the pre-existing tombstoned canonical survivor, not X's stale \
             requested id: {envelope}"
    );
    assert!(
        !envelope["results"][0]["result"]["deleted_at"].is_null(),
        "the rendered survivor must show its OWN tombstoned state (non-null deleted_at) \
             — absorbing a conflicting update must not resurrect it: {envelope}"
    );
    assert_eq!(
        envelope["results"][0]["result"]["weight"], 0.6,
        "ADR-039 DO NOTHING: the survivor keeps its own pre-existing weight, not X's \
             discarded patched weight (0.9): {envelope}"
    );
}

/// Acceptance test 2: every CLI-boundary rejection fires BEFORE any
/// write — each sub-case asserts both the error and that the db stays
/// empty (zero entities created).
#[tokio::test]
async fn atomic_cli_boundary_rejections_happen_before_any_write() {
    if crate::test_process::run_in_child() {
        return;
    }

    let khive_cfg = KhiveConfig::default();

    // (a) embedding-bearing verb.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op(
            "create",
            serde_json::json!({"kind": "concept", "name": "ShouldNotLand"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("embedding-bearing verb must be rejected");
        assert!(
            format!("{err:#}").contains("embedding-bearing"),
            "error: {err:#}"
        );
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
        assert_eq!(
            resp["results"][0]["result"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    // (b) read verb.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op("search", serde_json::json!({"query": "x"}))];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("read verbs must be rejected");
        assert!(format!("{err:#}").contains("read"), "error: {err:#}");
    }

    // (c) unlisted verb.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op("not_a_real_verb", serde_json::json!({}))];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("unlisted verbs must be rejected");
        assert!(
            format!("{err:#}").contains("not on the v1 atomic-admissible"),
            "error: {err:#}"
        );
    }

    // (d) op-count guard.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![
            atomic_op(
                "update",
                serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
            ),
            atomic_op(
                "update",
                serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
            ),
            atomic_op(
                "update",
                serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
            ),
        ];
        let err =
            crate::atomic_apply::execute_atomic_ops_file(ops, atomic_cfg(&db_path), &khive_cfg, 2)
                .await
                .expect_err("exceeding max_ops must be rejected");
        assert!(
            format!("{err:#}").contains("exceeds the configured maximum"),
            "error: {err:#}"
        );
    }

    // (e) governance verbs (`propose`/`review`/`withdraw`) — ADR-099 B3:
    // these are on the v1 admissible list
    // (ADR-099 D3 intends them to gain a seam) but have no prepare/apply
    // implementation in this slice yet. They must be rejected at this
    // SAME pre-runtime static guard — never reaching `KhiveRuntime::new`
    // or any write — not deferred to fail later inside `prepare_op`.
    for verb in ["propose", "review", "withdraw"] {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op(
            verb,
            serde_json::json!({"title": "x", "description": "y", "changeset": {}}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err(&format!("{verb:?} must be rejected before any write"));
        assert!(
            format!("{err:#}").contains("no --atomic prepare/apply seam"),
            "error for {verb:?}: {err:#}"
        );
        // No runtime/db file activity: the db stays empty (nothing else
        // touched it, so a plain re-open with the same path must show a
        // fresh, unwritten store).
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
        assert_eq!(
            resp["results"][0]["result"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "no write must have landed for {verb:?}"
        );
    }

    // (f) `merge` — ADR-099 B3: deferred at this SAME pre-runtime static guard rather than shipped
    // with partial parity. Must name the non-atomic merge verb as the
    // supported route, and must not reach `KhiveRuntime::new`/any write.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op(
            "merge",
            serde_json::json!({
                "into_id": uuid::Uuid::new_v4().to_string(),
                "from_id": uuid::Uuid::new_v4().to_string(),
            }),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("merge must be rejected before any write");
        assert!(
            format!("{err:#}").contains("use the non-atomic merge verb instead"),
            "error: {err:#}"
        );
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
        assert_eq!(
            resp["results"][0]["result"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "no write must have landed for merge"
        );
    }
}

// ── ADR-099 B3 fix: `--atomic` deny_unknown_fields parity ────────────────
//
// Canonical `update`/`delete`/`link`/`gtd.transition`/`gtd.complete`
// reject unknown/typo'd arg keys via `#[serde(deny_unknown_fields)]` on
// their param structs. Pre-fix, `--atomic` silently dropped unrecognized
// keys instead of rejecting the op — a typo like `conten` (for
// `content`) would report `ok:true` while quietly discarding the
// caller's intended change. These tests exercise the fix at the same
// `execute_atomic_ops_file` seam as the acceptance tests above, and are
// the end-to-end counterpart to the syntactic-only unit coverage in
// `atomic_apply::validate_atomic_args_tests`.

/// Sharp case called out explicitly: atomic `update(id=X,
/// conten="hello")` (typo of `content`) must be rejected AND must not
/// mutate the row — no `content` change, no `updated_at` bump. Pre-fix,
/// this silently discarded `conten`, reset every other field to its
/// current value, bumped `updated_at`, and reported `ok:true`.
#[tokio::test]
async fn atomic_update_entity_unknown_field_is_rejected_and_does_not_mutate_row() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (entity_id, updated_at_before) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"create(kind="concept", name="TypoGuardX", description="original")"#,
        )
        .await;
        let id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string();
        let get_resp = dispatch_json(&server, &format!(r#"get(id="{id}")"#)).await;
        let updated_at = get_resp["results"][0]["result"]["updated_at"].clone();
        (id, updated_at)
    };

    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": entity_id, "conten": "hello"}),
    )];
    let khive_cfg = KhiveConfig::default();
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `conten` must be rejected, not silently dropped");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    let server = isolated_server(&db_path);
    let get_resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert_eq!(
        get_resp["results"][0]["result"]["description"], "original",
        "a rejected op must not have mutated description: {get_resp}"
    );
    assert_eq!(
        get_resp["results"][0]["result"]["updated_at"], updated_at_before,
        "a rejected op must not bump updated_at (no write happened): {get_resp}"
    );
}

/// update-note variant of the same parity fix: a typo'd key on a note
/// update must be rejected, and a well-formed note update still
/// succeeds (parity boundary — don't over-reject).
#[tokio::test]
async fn atomic_update_note_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let note_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"create(kind="observation", content="original note")"#,
        )
        .await;
        resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string()
    };

    // (a) unknown field rejected.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": note_id, "conten": "typo'd"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `conten` on a note update must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed update still succeeds.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": note_id, "content": "updated note"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed note update must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let get_resp = dispatch_json(&server, &format!(r#"get(id="{note_id}")"#)).await;
    assert_eq!(
        get_resp["results"][0]["result"]["content"], "updated note",
        "the well-formed update must have landed: {get_resp}"
    );
}

#[tokio::test]
async fn atomic_task_note_update_projects_status_like_canonical_handlers() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let task_id = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"gtd.assign(title="AtomicProjectionTask", status="next")"#,
        )
        .await;
        response["results"][0]["result"]["full_id"]
            .as_str()
            .expect("task id")
            .to_string()
    };
    let khive_cfg = KhiveConfig::default();
    let update = || {
        vec![atomic_op(
            "update",
            serde_json::json!({"id": task_id.clone(), "content": "projected body"}),
        )]
    };

    let changed = crate::atomic_apply::execute_atomic_ops_file(
        update(),
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic note update");
    let changed_result = &changed["results"][0]["result"];
    assert_eq!(changed_result["status"], "next", "{changed}");
    assert_eq!(changed_result["lifecycle"], "active", "{changed}");
    assert_eq!(
        changed_result["display_name"], "AtomicProjectionTask",
        "{changed}"
    );
    assert!(changed_result.get("unchanged").is_none(), "{changed}");

    let server = isolated_server(&db_path);
    let canonical_get = dispatch_json(&server, &format!(r#"get(id="{task_id}")"#)).await;
    assert_eq!(changed_result, &canonical_get["results"][0]["result"]);

    let unchanged = crate::atomic_apply::execute_atomic_ops_file(
        update(),
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic note no-op update");
    let unchanged_result = &unchanged["results"][0]["result"];
    assert_eq!(unchanged_result["unchanged"], true, "{unchanged}");
    let canonical_update = dispatch_json(
        &server,
        &format!(r#"update(id="{task_id}", content="projected body")"#),
    )
    .await;
    assert_eq!(unchanged_result, &canonical_update["results"][0]["result"]);
}

/// `delete`: a typo'd key (`hardd` for `hard`) must be rejected before
/// any write; a well-formed delete still succeeds.
#[tokio::test]
async fn atomic_delete_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let entity_id = {
        let server = isolated_server(&db_path);
        let resp =
            dispatch_json(&server, r#"create(kind="concept", name="DeleteTypoGuard")"#).await;
        resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string()
    };

    // (a) unknown field rejected — entity must survive.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "delete",
        serde_json::json!({"id": entity_id, "hardd": true}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `hardd` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );
    let server = isolated_server(&db_path);
    let get_resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert!(
        get_resp["results"][0]["result"]["deleted_at"].is_null(),
        "a rejected delete must not have deleted the entity: {get_resp}"
    );

    // (b) well-formed delete still succeeds.
    let ops = vec![atomic_op("delete", serde_json::json!({"id": entity_id}))];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed delete must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// `link`: a typo'd key (`relatoin` for `relation`) must be rejected
/// before any write; a well-formed link still succeeds. (Distinct from
/// the Leo-accepted `target_backend` conflict-arm deferral — out of
/// scope here.)
#[tokio::test]
async fn atomic_link_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (a_id, b_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="LinkTypoA"), create(kind="concept", name="LinkTypoB")]"#,
        )
        .await;
        let a_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("a id")
            .to_string();
        let b_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("b id")
            .to_string();
        (a_id, b_id)
    };

    // (a) unknown field rejected.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "link",
        serde_json::json!({
            "source_id": a_id,
            "target_id": b_id,
            "relation": "extends",
            "relatoin": "extends",
        }),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `relatoin` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed link still succeeds.
    let ops = vec![atomic_op(
        "link",
        serde_json::json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed link must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// `gtd.transition`: a typo'd key (`notee` for `note`) must be rejected
/// before any write (task status unchanged); a well-formed transition
/// still succeeds.
#[tokio::test]
async fn atomic_gtd_transition_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let task_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"gtd.assign(title="TransitionTypoGuard", status="inbox")"#,
        )
        .await;
        // gtd.assign's `id` field is always the short hex form
        // (handlers.rs:372) regardless of presentation mode — use
        // `full_id`, the real UUID, so it round-trips through the
        // atomic prepare path's UUID parse.
        resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("full_id")
            .to_string()
    };

    // (a) unknown field rejected — status must stay "inbox".
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "gtd.transition",
        serde_json::json!({"id": task_id, "status": "next", "notee": "typo"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `notee` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed transition still succeeds.
    let ops = vec![atomic_op(
        "gtd.transition",
        serde_json::json!({"id": task_id, "status": "next"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed gtd.transition must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// `gtd.complete`: a typo'd key (`resutl` for `result`) must be
/// rejected before any write (task status unchanged); a well-formed
/// complete still succeeds.
#[tokio::test]
async fn atomic_gtd_complete_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let task_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"gtd.assign(title="CompleteTypoGuard", status="next")"#,
        )
        .await;
        // Same `full_id` note as the transition test above.
        resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("full_id")
            .to_string()
    };

    // (a) unknown field rejected.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "gtd.complete",
        serde_json::json!({"id": task_id, "resutl": "typo"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `resutl` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed complete still succeeds.
    let ops = vec![atomic_op(
        "gtd.complete",
        serde_json::json!({"id": task_id, "result": "shipped"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed gtd.complete must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

// ── ADR-099 B3: delete kind parity, update
// null/type validation, canonical id resolution, per-op result payloads ──

/// Atomic `delete(id=<entity>, kind="note")` must be
/// REJECTED (no row deleted) — pre-fix, atomic ignored `kind` entirely
/// and deleted the entity anyway (a destructive wrong-substrate action).
/// `delete(id=<entity>, kind="entity")` and `kind` omitted must both
/// still succeed.
#[tokio::test]
async fn atomic_delete_rejects_kind_mismatch_and_accepts_matching_or_omitted_kind() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let (mismatch_id, matching_id, omitted_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="KindMismatch"), create(kind="concept", name="KindMatching"), create(kind="concept", name="KindOmitted")]"#,
        )
        .await;
        let id = |i: usize| {
            resp["results"][i]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string()
        };
        (id(0), id(1), id(2))
    };

    // (a) kind mismatch: entity, caller says "note" — must be rejected,
    // entity must still be present afterward.
    let ops = vec![atomic_op(
        "delete",
        serde_json::json!({"id": mismatch_id, "kind": "note"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("delete(kind=\"note\") on an entity must be rejected");
    assert!(
        format!("{err:#}").contains("not found"),
        "expected a NotFound-shaped rejection, error: {err:#}"
    );
    let server = isolated_server(&db_path);
    let resp = dispatch_json(&server, &format!(r#"get(id="{mismatch_id}")"#)).await;
    assert!(
        resp["results"][0]["result"]["deleted_at"].is_null(),
        "entity must NOT be deleted after a kind-mismatch rejection: {resp}"
    );

    // (b) matching kind: succeeds.
    let ops = vec![atomic_op(
        "delete",
        serde_json::json!({"id": matching_id, "kind": "entity"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("delete(kind=\"entity\") on an entity must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    // (c) omitted kind: succeeds.
    let ops = vec![atomic_op("delete", serde_json::json!({"id": omitted_id}))];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("delete with kind omitted must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// Canonical and atomic updates preserve nullable-field presence and applicability.
/// See `crates/kkernel/docs/design.md#execrs-regression-test-notes`.
#[tokio::test]
async fn atomic_update_null_and_type_semantics_match_canonical_behavior() {
    if crate::test_process::run_in_child() {
        return;
    }

    async fn call(db_path: &str, ops: &str) -> serde_json::Value {
        let server = isolated_server(db_path);
        let envelope = dispatch_json(&server, ops).await;
        assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
        envelope["results"][0]["result"].clone()
    }

    async fn get(db_path: &str, id: &str) -> serde_json::Value {
        call(db_path, &format!(r#"get(id="{id}")"#)).await
    }

    async fn update(db_path: &str, atomic: bool, args: serde_json::Value) -> Result<(), String> {
        let envelope = if atomic {
            crate::atomic_apply::execute_atomic_ops_file(
                vec![atomic_op("update", args)],
                atomic_cfg(db_path),
                &KhiveConfig::default(),
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            )
            .await
            .map_err(|error| format!("{error:#}"))?
        } else {
            let fields = args
                .as_object()
                .expect("update argument object")
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ");
            let server = isolated_server(db_path);
            dispatch_json(&server, &format!("update({fields})")).await
        };
        let accepted = if atomic {
            envelope["atomic"]["committed"] == true
        } else {
            envelope["results"][0]["ok"] == true
        };
        if accepted {
            Ok(())
        } else {
            Err(envelope.to_string())
        }
    }

    for atomic in [false, true] {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8");
        let created = call(
            db_path,
            r#"create(kind="concept", name="NullSemantics", description="orig-desc", properties={"k":"v"}, tags=["a","b"], skip_dedup_check=true)"#,
        ).await;
        let id = created["id"].as_str().expect("full entity id");
        let initial = get(db_path, id).await;
        assert_eq!(initial["description"], "orig-desc");

        let error = update(db_path, atomic, serde_json::json!({"id":id,"name":123}))
            .await
            .expect_err("non-string name stays invalid");
        assert!(error.contains("name must be a string"), "{error}");
        assert_eq!(get(db_path, id).await, initial);

        for field in ["salience", "decay_factor"] {
            let mut args = serde_json::json!({"id":id,"name":"must-not-land"});
            args[field] = serde_json::Value::Null;
            let error = update(db_path, atomic, args)
                .await
                .expect_err("nullable note fields are inapplicable to entities");
            assert!(
                error.contains(&format!("field '{field}' is not valid for an entity")),
                "{error}"
            );
            assert_eq!(get(db_path, id).await, initial);
        }

        for malformed in [
            serde_json::json!(123),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let error = update(
                db_path,
                atomic,
                serde_json::json!({
                    "id":id,"description":malformed,"name":"must-not-land"
                }),
            )
            .await
            .expect_err("non-string description stays invalid");
            let expected = if atomic {
                "description must be a string or null"
            } else {
                "description must be null or a string"
            };
            assert!(error.contains(expected), "{error}");
            assert_eq!(get(db_path, id).await, initial);
        }

        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"properties":{"added":true}}),
        )
        .await
        .expect("omitted description preserves its value");
        let omitted = get(db_path, id).await;
        assert_eq!(omitted["description"], "orig-desc");
        assert_eq!(omitted["properties"]["added"], true);

        update(
            db_path,
            atomic,
            serde_json::json!({
                "id":id,"name":null,"description":null,"properties":null,"tags":null
            }),
        )
        .await
        .expect("description null clears; other null fields keep their behavior");
        let cleared = get(db_path, id).await;
        assert_eq!(cleared.get("description"), Some(&serde_json::Value::Null));
        assert_eq!(cleared["name"], "NullSemantics");
        assert_eq!(cleared["properties"], omitted["properties"]);
        assert_eq!(cleared["tags"], omitted["tags"]);

        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"name":"after-clear"}),
        )
        .await
        .expect("an unrelated update preserves a cleared description");
        let after_clear = get(db_path, id).await;
        assert_eq!(
            after_clear.get("description"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(after_clear["name"], "after-clear");

        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"description":"reset"}),
        )
        .await
        .expect("description may be set again");
        assert_eq!(get(db_path, id).await["description"], "reset");
        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"name":"after-reset"}),
        )
        .await
        .expect("omission preserves a newly set description");
        assert_eq!(get(db_path, id).await["description"], "reset");
        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"description":""}),
        )
        .await
        .expect("empty string remains a concrete description");
        assert_eq!(
            get(db_path, id).await.get("description"),
            Some(&serde_json::json!(""))
        );

        let note = call(
            db_path,
            r#"create(kind="observation", name="kept-note", content="before")"#,
        )
        .await;
        let note_id = note["id"].as_str().expect("full note id");
        update(
            db_path,
            atomic,
            serde_json::json!({"id":note_id,"salience":0.6,"decay_factor":0.03}),
        )
        .await
        .expect("nullable fields remain valid on notes");
        let note_before = get(db_path, note_id).await;
        assert_eq!(note_before["salience"], 0.6);
        assert_eq!(note_before["decay_factor"], 0.03);
        let error = update(
            db_path,
            atomic,
            serde_json::json!({
                "id":note_id,"description":null,"content":"must-not-land"
            }),
        )
        .await
        .expect_err("a present description is inapplicable to notes");
        assert!(
            error.contains("field 'description' is not valid for a note"),
            "{error}"
        );
        assert_eq!(get(db_path, note_id).await, note_before);
        update(
            db_path,
            atomic,
            serde_json::json!({
                "id":note_id,"content":"after","name":null,"salience":null,"decay_factor":null
            }),
        )
        .await
        .expect("valid note clears still accept null name as unchanged");
        let note_after = get(db_path, note_id).await;
        assert_eq!(note_after["content"], "after");
        assert_eq!(note_after["name"], note_before["name"]);
        assert_eq!(note_after.get("salience"), Some(&serde_json::Value::Null));
        assert_eq!(
            note_after.get("decay_factor"),
            Some(&serde_json::Value::Null)
        );

        let target = call(
            db_path,
            r#"create(kind="concept", name="DescriptionTarget", skip_dedup_check=true)"#,
        )
        .await;
        let target_id = target["id"].as_str().expect("full target id");
        let edge = call(db_path, &format!(r#"link(source_id="{id}", target_id="{target_id}", relation="supports", weight=0.4)"#)).await;
        let edge_id = edge["id"].as_str().expect("full edge id");
        let edge_before = get(db_path, edge_id).await;
        for field in ["description", "salience", "decay_factor"] {
            let mut args = serde_json::json!({"id":edge_id,"weight":0.8});
            args[field] = serde_json::Value::Null;
            let error = update(db_path, atomic, args)
                .await
                .expect_err("nullable entity/note fields are inapplicable to edges");
            assert!(
                error.contains(&format!("field '{field}' is not valid for an edge")),
                "{error}"
            );
            assert_eq!(get(db_path, edge_id).await, edge_before);
        }
        update(
            db_path,
            atomic,
            serde_json::json!({"id":edge_id,"weight":0.8,"name":null}),
        )
        .await
        .expect("valid edge update preserves null name behavior");
        assert_eq!(get(db_path, edge_id).await["weight"], 0.8);
    }
}
/// An atomic ops-file using an 8-hex-prefix id for
/// `update` AND `gtd.transition` must succeed identically to canonical
/// (which accepts full UUID or an 8+ hex prefix); a non-existent prefix
/// must error with canonical's error shape ("no record matches
/// prefix"). Pre-fix, atomic did a bare `Uuid::parse_str` and rejected
/// any short id outright — the same ops-file that succeeds non-atomically
/// (e.g. against `gtd.assign`'s own short `id` output) would fail before
/// prepare under `--atomic`.
#[tokio::test]
async fn atomic_update_and_gtd_transition_accept_8_hex_prefix_ids() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let (entity_full_id, task_full_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"create(kind="concept", name="PrefixEntity")"#).await;
        let entity_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("entity id")
            .to_string();
        let resp = dispatch_json(&server, r#"gtd.assign(title="PrefixTask", status="next")"#).await;
        let task_id = resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("task full_id")
            .to_string();
        (entity_id, task_id)
    };
    let entity_prefix = &entity_full_id[..8];
    let task_prefix = &task_full_id[..8];

    // (a) 8-hex-prefix update and gtd.transition in the SAME atomic unit
    // both succeed.
    let ops = vec![
        atomic_op(
            "update",
            serde_json::json!({"id": entity_prefix, "name": "PrefixEntity-renamed"}),
        ),
        atomic_op(
            "gtd.transition",
            serde_json::json!({"id": task_prefix, "status": "active"}),
        ),
    ];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("8-hex-prefix ids must resolve identically to canonical");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let resp = dispatch_json(&server, &format!(r#"get(id="{entity_full_id}")"#)).await;
    assert_eq!(
        resp["results"][0]["result"]["name"], "PrefixEntity-renamed",
        "prefix-addressed update must have landed: {resp}"
    );

    // (b) a non-existent 8-hex prefix errors with canonical's error
    // shape.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": "deadbeef", "name": "should not resolve"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("a non-existent prefix must be rejected");
    assert!(
        format!("{err:#}").contains("no record matches prefix"),
        "error: {err:#}"
    );
}

include!("exec_atomic_result_shape_tests.rs");
include!("exec_atomic_kind_tests.rs");
