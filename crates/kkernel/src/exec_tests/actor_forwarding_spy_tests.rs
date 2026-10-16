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
