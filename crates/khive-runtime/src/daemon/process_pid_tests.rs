// Focused regression tests for the unsafe process probe (SAFETY: signal 0
// is an existence check with no side effects; see is_process_running).

#[test]
fn current_process_is_running() {
    // The current PID is always alive.
    let pid = std::process::id();
    assert!(
        is_process_running(pid),
        "current process {pid} should be detected as running"
    );
}

#[test]
fn pid_zero_is_not_running() {
    // PID 0 is the process group; kill(0, 0) sends to the group,
    // which we treat as invalid — the guard `pid <= 0` must block it.
    assert!(
        !is_process_running(0),
        "pid 0 must be rejected by the guard before the unsafe call"
    );
}

#[test]
fn very_large_pid_is_not_running() {
    // u32::MAX overflows i32 — try_from returns Err, guard returns false.
    assert!(
        !is_process_running(u32::MAX),
        "u32::MAX should fail i32 conversion and return false"
    );
}

// EPERM (process exists, no permission to signal it) must not be
// misread as "not running" during stale-daemon cleanup.

#[test]
fn classify_kill_result_zero_is_alive() {
    assert_eq!(classify_kill_result(0, 0), PidLiveness::Alive);
    assert!(classify_kill_result(0, 0).is_running());
}

#[test]
fn classify_kill_result_esrch_is_dead() {
    assert_eq!(classify_kill_result(-1, libc::ESRCH), PidLiveness::Dead);
    assert!(!classify_kill_result(-1, libc::ESRCH).is_running());
}

#[test]
fn classify_kill_result_eperm_is_permission_denied_and_counts_as_running() {
    assert_eq!(
        classify_kill_result(-1, libc::EPERM),
        PidLiveness::PermissionDenied
    );
    assert!(
        classify_kill_result(-1, libc::EPERM).is_running(),
        "EPERM must be unknown-safe: treated as running, never as a basis \
             for stale cleanup to unlink a live daemon's rendezvous files"
    );
}

#[test]
fn same_process_pid_requires_explicit_in_process_harness_opt_in() {
    let current = std::process::id();
    assert!(
        !pid_can_name_incumbent(current, current, false),
        "production startup must not trust a same-PID stale rendezvous"
    );
    assert!(
        pid_can_name_incumbent(current, current, true),
        "the in-process harness must let a live same-PID owner win"
    );
    // Keep the probe two away from `current` so the fixture preserves the
    // off-by-one regression check for adjacent PIDs; wrapping_add avoids
    // making the test overflow-sensitive at the u32 boundary.
    let distinct_probe_pid = current.wrapping_add(2);
    assert_ne!(
        distinct_probe_pid, current,
        "probe PID must differ from this process's PID"
    );
    assert!(
        pid_can_name_incumbent(distinct_probe_pid, current, false),
        "a distinct PID remains eligible under ordinary production rules"
    );
}

#[test]
fn pid_1_probe_is_running_regardless_of_permission_outcome() {
    // PID 1 (init/launchd) always exists. An unprivileged process gets
    // EPERM signaling it (never ESRCH); running as root would get 0
    // instead. Either way `is_process_running` must report true — this
    // is the live regression guard for EPERM being misread as dead;
    // `classify_kill_result` above is the pure-function unit coverage
    // for the same mapping, kept independent of process ownership so
    // it is never flaky in CI.
    assert!(
        is_process_running(1),
        "PID 1 always exists; EPERM must not read as dead"
    );
}

#[tokio::test]
async fn stale_cleanup_preserves_live_incumbent_without_reachable_socket() {
    // No other test in this process may fork while this fixture briefly
    // owns a listener: a child that inherits it can keep the socket
    // reachable after this test drops its own descriptor.
    if crate::test_process::run_in_child() {
        return;
    }
    assert_eq!(
        std::env::var("KHIVE_RUNTIME_ISOLATED_TEST").ok().as_deref(),
        Some("daemon::tests::stale_cleanup_preserves_live_incumbent_without_reachable_socket"),
        "the stale-listener fixture must run alone in its child process"
    );
    for socket_exists in [false, true] {
        let dir = crate::test_process::private_tempdir_builder()
            .tempdir()
            .expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        if socket_exists {
            let listener = std::os::unix::net::UnixListener::bind(&sock)
                .expect("bind socket before closing listener");
            drop(listener);
        }
        let identity = socket_identity(&sock);
        assert_eq!(identity.is_some(), socket_exists);
        let error = UnixStream::connect(&sock)
            .await
            .expect_err("incumbent must have no reachable listener");
        assert_eq!(
            error.kind(),
            if socket_exists {
                std::io::ErrorKind::ConnectionRefused
            } else {
                std::io::ErrorKind::NotFound
            }
        );
        let live_pid = std::process::id().to_string();
        let _pid_file_guard = write_pid_file_exclusive(&pid_file)
            .expect("claim and lock the live incumbent PID file");

        // Harness eligibility makes our own stable PID an incumbent;
        // ordinary same-PID rejection is covered separately above.
        assert!(
            matches!(
                cleanup_stale_daemon(&sock, &pid_file, true, "probe-test").await,
                Incumbent::Live(_) | Incumbent::Serving(_)
            ),
            "live incumbent must retain ownership with socket_exists={socket_exists}"
        );
        assert_eq!(
            std::fs::read_to_string(&pid_file).expect("live incumbent PID must survive"),
            live_pid
        );
        assert!(socket_identity(&sock) == identity);
    }
}

#[tokio::test]
#[serial]
async fn live_foreign_pid_does_not_block_daemon_startup() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

    let stale_listener =
        std::os::unix::net::UnixListener::bind(&sock).expect("create stale socket path");
    drop(stale_listener);

    let mut foreign = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn live unrelated process");
    std::fs::write(&pid_file, foreign.id().to_string()).expect("write unrelated PID");

    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "foreign-pid-start-test".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let daemon = tokio::spawn(run_daemon_in_process_test(dispatcher));
    let connected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = UnixStream::connect(&sock).await {
                break Some(stream);
            }
            if daemon.is_finished() {
                break None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;

    let response = if let Ok(Some(mut stream)) = connected {
        let mut request = base_request_frame("foreign-pid-start-test");
        request.probe_only = true;
        let payload = serde_json::to_vec(&request).expect("encode probe request");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            write_frame(&mut stream, &payload).await.ok()?;
            let raw = read_frame(&mut stream).await.ok()?;
            serde_json::from_slice::<DaemonResponseFrame>(&raw).ok()
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let foreign_survived_start = foreign
        .try_wait()
        .expect("query unrelated process state")
        .is_none();

    daemon.abort();
    let _ = daemon.await;
    let _ = foreign.kill();
    let _ = foreign.wait();
    std::env::remove_var("KHIVE_SOCKET");
    std::env::remove_var("KHIVE_PID");
    std::env::remove_var("KHIVE_LOCK");

    assert!(
        response.is_some_and(|response| {
            response.ok && response.served_config_id.as_deref() == Some("foreign-pid-start-test")
        }),
        "daemon must start and answer its identity probe"
    );
    assert!(
        foreign_survived_start,
        "starting khived must leave the unrelated live process running"
    );
}

#[tokio::test]
#[serial]
async fn second_start_refuses_while_pid_file_is_locked_before_bind() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = crate::test_process::private_tempdir_builder()
        .tempdir()
        .expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

    let _incumbent_startup_guard = write_pid_file_exclusive(&pid_file)
        .expect("incumbent claims and locks its PID file before binding");
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "startup-lock-test".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        run_daemon_in_process_test(dispatcher),
    )
    .await;
    let refused = matches!(second, Ok(Err(_)));
    let pid_file_survived = pid_file.exists();
    let socket_was_not_bound = !sock.exists();

    std::env::remove_var("KHIVE_SOCKET");
    std::env::remove_var("KHIVE_PID");
    std::env::remove_var("KHIVE_LOCK");

    assert!(
        refused,
        "a second start must refuse while an incumbent holds its pre-bind PID lock"
    );
    assert!(
        pid_file_survived,
        "the incumbent PID file must remain in place"
    );
    assert!(
        socket_was_not_bound,
        "the second start must not bind the socket"
    );
}

#[test]
fn env_truthy_recognises_set_values() {
    assert!(!env_truthy("__KHIVE_TEST_ABSENT_VAR_XYZ__"));

    // env_truthy with a live value — set and unset atomically to avoid
    // cross-test pollution (not parallel-safe without serial_test, but these
    // are fast unit tests and the variable name is unique).
    let key = "__KHIVE_TEST_TRUTHY_ABC__";
    std::env::set_var(key, "1");
    assert!(env_truthy(key));
    std::env::set_var(key, "false");
    assert!(!env_truthy(key));
    std::env::set_var(key, "0");
    assert!(!env_truthy(key));
    std::env::remove_var(key);
}
