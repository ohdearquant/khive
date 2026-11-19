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
