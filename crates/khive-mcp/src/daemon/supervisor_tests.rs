// Included by daemon::tests::handover, which supplies isolated rendezvous paths.

fn marker_path() -> std::path::PathBuf {
    std::env::var("KHIVE_SUPERVISOR_MARKER")
        .expect("isolate() must set KHIVE_SUPERVISOR_MARKER")
        .into()
}

fn write_marker(path: &std::path::Path, job: &str, pid: u32, interval: Option<u64>) {
    let mut contents = format!("{job}\n{pid}\n");
    if let Some(interval) = interval {
        contents.push_str(&format!("{interval}\n"));
    }
    let temporary = path.with_extension("next");
    std::fs::write(&temporary, contents).expect("write supervision marker");
    std::fs::rename(temporary, path).expect("publish supervision marker");
}

fn reaped_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn short-lived child");
    let pid = child.id();
    assert!(child.wait().unwrap().success());
    assert!(!process_is_alive(pid));
    pid
}

fn assert_starting(result: Option<Result<String, McpError>>, job: &str) {
    let error = result
        .expect("no local fallback")
        .expect_err("still starting");
    assert!(error.message.contains(job));
    assert!(error.message.contains("starting"));
    let data = error.data.expect("retry data");
    assert_eq!(data["reason"], "supervised_daemon_starting");
    assert_eq!(data["retryable"], true);
}

#[test]
#[serial]
fn supervisor_marker_parses_interval_and_bounds_legacy_or_invalid_values() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    assert!(read_supervisor_marker().is_none());
    let started = tokio::time::Instant::now();
    for (third_line, bound) in [
        ("", 30),
        ("1\n", 3),
        ("7\n", 21),
        ("0\n", 30),
        ("invalid\n", 30),
        ("18446744073709551615\n", 30),
        ("18446744073709551616\n", 30),
    ] {
        std::fs::write(marker_path(), format!("example.job\n42\n{third_line}")).unwrap();
        let marker = read_supervisor_marker().unwrap();
        assert_eq!(marker.job, "example.job");
        assert_eq!(marker.pid, 42);
        assert!(marker.modified.is_some());
        assert_eq!(
            marker.wait_deadline(started) - started,
            Duration::from_secs(bound)
        );
    }
}

#[tokio::test]
#[serial]
async fn no_marker_reaches_the_bootstrap_spawn_attempt() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let attempts = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        attempts.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        forward_or_spawn_with(&request("stats()"), &spawn),
    )
    .await
    .unwrap();
    assert_eq!(
        result.unwrap().unwrap_err().data.unwrap()["reason"],
        "respawn_failed"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

async fn assert_marker_eventually_bootstraps(pid: u32) {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    write_marker(&marker_path(), "example.job", pid, Some(1));
    let marker_before = std::fs::read(marker_path()).unwrap();
    assert!(
        !pid_path().exists(),
        "a supervisor marker is not a daemon PID file"
    );
    let attempts = AtomicUsize::new(0);
    let started = tokio::time::Instant::now();
    let spawn = || -> std::io::Result<std::process::Child> {
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "no early bootstrap"
        );
        attempts.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        forward_or_spawn_with(&request("stats()"), &spawn),
    )
    .await
    .unwrap();
    assert_eq!(
        result.unwrap().unwrap_err().data.unwrap()["reason"],
        "respawn_failed"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    // First bootstrap and stale recovery share one guarded entry. With no PID
    // file that entry must not signal the PID from the supervisor declaration.
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 1);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read(marker_path()).unwrap(),
        marker_before,
        "clients must not change launcher markers"
    );
}

#[tokio::test]
#[serial]
async fn dead_supervisor_marker_waits_three_intervals_then_bootstraps() {
    assert_marker_eventually_bootstraps(reaped_pid()).await;
}

#[tokio::test]
#[serial]
async fn live_or_reused_supervisor_pid_cannot_suppress_bootstrap_forever() {
    assert_marker_eventually_bootstraps(std::process::id()).await;
}

#[tokio::test]
#[serial]
async fn supervisor_caller_deadline_returns_retryable_starting_without_recovery() {
    let _cleanup = RecoveryTestGuard::new();
    for pid in [reaped_pid(), std::process::id()] {
        let dir = tempfile::tempdir().unwrap();
        isolate(dir.path());
        // A two-line legacy marker retains its ten-second restart interval.
        write_marker(&marker_path(), "example.job", pid, None);
        let started = tokio::time::Instant::now();
        let result = khive_storage::scope_request_read_deadline(
            Duration::from_millis(200),
            forward_or_spawn_with(&request("stats()"), &never_spawn),
        )
        .await;
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_starting(result, "example.job");
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
        assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
#[serial]
async fn supervisor_caller_cancellation_prevents_recovery() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    write_marker(&marker_path(), "example.job", std::process::id(), Some(1));
    let (tx, rx) = tokio::sync::watch::channel(false);
    let frame = request("stats()");
    let forward = khive_storage::scope_request_read_cancellation(
        rx,
        forward_or_spawn_with(&frame, &never_spawn),
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(true).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(forward, cancel)
    })
    .await
    .unwrap();
    assert_starting(result, "example.job");
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial]
async fn disappearing_supervisor_marker_resumes_unmanaged_bootstrap() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    write_marker(&marker_path(), "example.job", std::process::id(), Some(10));
    let attempts = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        assert!(!marker_path().exists());
        attempts.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let frame = request("stats()");
    let forward = forward_or_spawn_with(&frame, &spawn);
    let remove = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::fs::remove_file(marker_path()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(forward, remove)
    })
    .await
    .unwrap();
    assert_eq!(
        result.unwrap().unwrap_err().data.unwrap()["reason"],
        "respawn_failed"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial]
async fn supervisor_respawn_gap_and_serving_incumbent_forward_without_spawning() {
    let _cleanup = RecoveryTestGuard::new();
    for (pid, bind_delay) in [
        (reaped_pid(), Duration::from_millis(150)),
        (std::process::id(), Duration::from_millis(150)),
        (std::process::id(), Duration::ZERO),
    ] {
        let dir = tempfile::tempdir().unwrap();
        isolate(dir.path());
        write_marker(&marker_path(), "example.job", pid, Some(1));
        let sock = socket_path();
        // Bind the incumbent before forwarding; delayed cases bind during the wait.
        let existing = if bind_delay.is_zero() {
            Some(tokio::net::UnixListener::bind(&sock).unwrap())
        } else {
            None
        };
        let peer = async move {
            let listener = match existing {
                Some(listener) => listener,
                None => {
                    tokio::time::sleep(bind_delay).await;
                    tokio::net::UnixListener::bind(&sock).unwrap()
                }
            };
            let (mut stream, _) = listener.accept().await.unwrap();
            let frame: DaemonRequestFrame =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert!(
                !frame.probe_only,
                "startup wait must not enter lifecycle recovery"
            );
            write_frame(
                &mut stream,
                &serde_json::to_vec(&frame_ok("supervisor-ok")).unwrap(),
            )
            .await
            .unwrap();
        };
        let frame = request("stats()");
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(forward_or_spawn_with(&frame, &never_spawn), peer)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap().unwrap(), "supervisor-ok");
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
        assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
#[serial]
async fn legacy_two_line_dead_pid_marker_waits_default_three_intervals() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    write_marker(&marker_path(), "example.legacy", reaped_pid(), None);
    let started = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(31),
        wait_for_supervisor(
            &request("stats()"),
            &mut ReadReplayBudget::new(false),
            started,
            read_supervisor_marker().unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, ForwardOutcome::NoSocket));
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
#[serial]
async fn unreadable_supervisor_marker_has_a_finite_default_budget() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    // A directory at the marker path is unreadable as marker text, even as root.
    std::fs::create_dir(marker_path()).unwrap();
    let marker = read_supervisor_marker().unwrap();
    assert_eq!(marker.job, "<unreadable>");
    let started = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(31),
        wait_for_supervisor(
            &request("stats()"),
            &mut ReadReplayBudget::new(false),
            started,
            marker,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, ForwardOutcome::NoSocket));
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert!(marker_path().is_dir());
}

#[tokio::test(start_paused = true)]
#[serial]
async fn supervisor_budget_is_anchored_before_marker_discovery() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let started = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(2)).await;
    write_marker(&marker_path(), "example.job", std::process::id(), Some(1));
    let outcome = wait_for_supervisor(
        &request("stats()"),
        &mut ReadReplayBudget::new(false),
        started,
        read_supervisor_marker().unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, ForwardOutcome::NoSocket));
    assert_eq!(started.elapsed(), Duration::from_secs(3));
}

#[derive(Clone, Default)]
struct SupervisorLogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SupervisorLogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SupervisorLogBuffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
#[serial]
async fn crash_loop_marker_rewrites_cannot_reset_bound_and_log_latest_owner() {
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    write_marker(&marker_path(), "first.job", reaped_pid(), Some(1));
    assert!(
        !pid_path().exists(),
        "only the supervisor marker carries a PID"
    );
    let logs = SupervisorLogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(logs.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let started = tokio::time::Instant::now();
    let attempts = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        assert!(started.elapsed() >= Duration::from_secs(3));
        attempts.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let mut rewrites = 0;
    let rewrite = async {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Change both PID and interval: neither a live reused PID nor a
            // freshly written marker may prolong this request's original bound.
            write_marker(&marker_path(), "latest.job", std::process::id(), Some(10));
            rewrites += 1;
        }
    };
    let frame = request("stats()");
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::select! {
            result = forward_or_spawn_with(&frame, &spawn) => result,
            () = rewrite => unreachable!("rewriter runs until forwarding finishes"),
        }
    })
    .await
    .unwrap();
    assert_eq!(
        result.unwrap().unwrap_err().data.unwrap()["reason"],
        "respawn_failed"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(rewrites >= 5);
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let degradation = logs
        .lines()
        .find(|line| line.contains("supervisor present, daemon absent"))
        .expect("degraded bootstrap must be logged");
    assert!(degradation.contains("job=latest.job"), "{degradation}");
    assert!(
        degradation.contains(&format!("pid={}", std::process::id())),
        "{degradation}"
    );
    assert!(degradation.contains("pid_alive=true"), "{degradation}");
    assert!(
        degradation.contains("marker_age_secs=Some("),
        "{degradation}"
    );
    assert!(degradation.contains("time_waited_secs="), "{degradation}");
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 1);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
}
