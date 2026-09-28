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

fn publish_marker_with_launcher_lock(
    path: &std::path::Path,
    job: &str,
    pid: u32,
    interval: Option<u64>,
) {
    let launcher_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(supervisor_marker_lock_path(path))
        .unwrap();
    launcher_lock.lock().unwrap();
    write_marker(path, job, pid, interval);
    drop(launcher_lock);
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

#[tokio::test(start_paused = true)]
#[serial]
async fn client_blocked_on_marker_lock_returns_retryable_starting() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let held_lock = acquire_supervisor_marker_lock().await.unwrap();
    let spawn_calls = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        spawn_calls.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let frame = request("stats()");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        khive_storage::scope_request_read_deadline(
            Duration::from_millis(250),
            forward_or_spawn_with(&frame, &spawn),
        ),
    )
    .await
    .expect("marker lock wait is bounded")
    .expect("no local fallback")
    .expect_err("no daemon answered before the lock wait ended");
    let data = result.data.expect("retry data");
    assert_eq!(data["reason"], "supervised_daemon_starting");
    assert_eq!(data["retryable"], true);
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    assert!(!pid_path().exists());
    drop(held_lock);
}

#[test]
#[serial]
fn supervisor_marker_parses_interval_and_bounds_legacy_or_invalid_values() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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

#[tokio::test]
#[serial]
async fn client_holds_launcher_marker_lock_through_spawn_admission() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let attempts = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        attempts.fetch_add(1, Ordering::SeqCst);
        let competing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(supervisor_marker_lock_path(&marker_path()))
            .unwrap();
        assert!(
            matches!(competing.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "SPAWN_ADMISSION_HOLDS_LAUNCHER_LOCK: launcher cannot publish before spawn decision"
        );
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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    assert_marker_eventually_bootstraps(reaped_pid()).await;
}

#[tokio::test]
#[serial]
async fn live_or_reused_supervisor_pid_cannot_suppress_bootstrap_forever() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    assert_marker_eventually_bootstraps(std::process::id()).await;
}

#[tokio::test]
#[serial]
async fn supervisor_caller_deadline_returns_retryable_starting_without_recovery() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    assert!(matches!(outcome.outcome, ForwardOutcome::NoSocket));
    assert!(outcome.degraded_bootstrap);
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
#[serial]
async fn unreadable_supervisor_marker_has_a_finite_default_budget() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    assert!(matches!(outcome.outcome, ForwardOutcome::NoSocket));
    assert!(outcome.degraded_bootstrap);
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert!(marker_path().is_dir());
}

#[tokio::test(start_paused = true)]
#[serial]
async fn supervisor_budget_is_anchored_before_marker_discovery() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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
    assert!(matches!(outcome.outcome, ForwardOutcome::NoSocket));
    assert!(outcome.degraded_bootstrap);
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
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

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

#[test]
#[serial]
fn supervisor_fifo_marker_is_unreadable_without_a_writer() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    use std::os::unix::ffi::OsStrExt;

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let path = std::ffi::CString::new(marker_path().as_os_str().as_bytes()).unwrap();
    // SAFETY: the NUL-terminated path belongs to this fixture and remains live.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    // No writer opens this FIFO: a blocking read-open could never return.
    let marker = read_supervisor_marker().expect("FIFO remains a declaration");
    assert_eq!(marker.job, "<unreadable>");
    assert_eq!(marker.pid, 0);
    assert_eq!(marker.restart_interval, DEFAULT_SUPERVISOR_RESTART_INTERVAL);
}

#[test]
#[serial]
fn supervisor_symlink_marker_is_unreadable_even_with_a_valid_target() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let target = dir.path().join("other-marker");
    std::fs::write(&target, "another.job\n42\n1\n").unwrap();
    std::os::unix::fs::symlink(&target, marker_path()).unwrap();
    let marker = read_supervisor_marker().expect("symlink remains a declaration");
    assert_eq!(marker.job, "<unreadable>");
    assert_eq!(marker.pid, 0);
    assert_eq!(marker.restart_interval, DEFAULT_SUPERVISOR_RESTART_INTERVAL);
}

struct LateMarkerOwner(std::process::Child);

impl LateMarkerOwner {
    fn spawn() -> Self {
        Self(std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap())
    }

    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for LateMarkerOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct LateMarkerHookGuard {
    wait_started: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
}

impl LateMarkerHookGuard {
    fn publish_at(point: SupervisorDiscoveryPoint, path: std::path::PathBuf, pid: u32) -> Self {
        let wait_started = Arc::new(std::sync::Mutex::new(None));
        let observed = wait_started.clone();
        let mut hook = SUPERVISOR_DISCOVERY_HOOK.lock().unwrap();
        assert!(hook.is_none(), "isolated discovery hook");
        *hook = Some((point, Box::new(move || {
            assert!(!path.exists(), "marker must be absent on the initial probe");
            assert!(process_is_alive(pid), "the fixture's old owner must still be alive");
            assert!(!socket_path().exists(), "the socket must remain unbound");
            publish_marker_with_launcher_lock(&path, "late.supervisor", pid, Some(10));
            *SUPERVISOR_DISCOVERY_HOOK.lock().unwrap() = Some((
                SupervisorDiscoveryPoint::SupervisorWait,
                Box::new(move || { *observed.lock().unwrap() = Some(tokio::time::Instant::now()); }),
            ));
        })));
        Self { wait_started }
    }
}

impl Drop for LateMarkerHookGuard {
    fn drop(&mut self) {
        SUPERVISOR_DISCOVERY_HOOK.lock().unwrap().take();
    }
}

struct DiscoveryHookCleanup;

impl Drop for DiscoveryHookCleanup {
    fn drop(&mut self) {
        SUPERVISOR_DISCOVERY_HOOK.lock().unwrap().take();
    }
}

fn assert_late_supervisor_waited(
    result: Result<Option<Result<String, McpError>>, tokio::time::error::Elapsed>,
    marker: &str,
) {
    assert!(result.is_ok(), "{marker}: request must finish at its caller deadline");
    let result = result.unwrap();
    assert!(result.is_some(), "{marker}: no local fallback while the supervisor owns startup");
    let result = result.unwrap();
    assert!(result.is_err(), "{marker}: no socket has been bound");
    let error = result.unwrap_err();
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("reason")),
        Some(&serde_json::json!("supervised_daemon_starting")),
        "{marker}: late marker must enter the existing supervisor wait: {error:?}"
    );
    assert!(error.message.contains("late.supervisor"), "{marker}: report the discovered owner");
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0, "{marker}: no lifecycle recovery");
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0, "{marker}: no incumbent signal");
}

#[tokio::test(start_paused = true)]
#[serial]
async fn supervisor_published_during_unmanaged_retry_owns_caller_deadline() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let mut owner = LateMarkerOwner::spawn();
    assert!(owner.0.try_wait().unwrap().is_none());
    std::fs::write(pid_path(), owner.id().to_string()).unwrap();
    let started = tokio::time::Instant::now();
    let hook = LateMarkerHookGuard::publish_at(
        SupervisorDiscoveryPoint::UnmanagedRetry,
        marker_path(),
        owner.id(),
    );
    let spawn = || -> std::io::Result<std::process::Child> {
        Err(std::io::ErrorKind::NotFound.into())
    };
    let frame = request("stats()");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        khive_storage::scope_request_read_deadline(
            Duration::from_millis(250),
            forward_or_spawn_with(&frame, &spawn),
        ),
    ).await;
    assert!(marker_path().exists(), "UNMANAGED_RETRY_REREADS_SUPERVISOR: publication seam reached");
    assert_late_supervisor_waited(result, "UNMANAGED_RETRY_REREADS_SUPERVISOR");
    assert!(hook.wait_started.lock().unwrap().is_some_and(|at| at - started < Duration::from_millis(200)),
        "UNMANAGED_RETRY_REREADS_SUPERVISOR: enter supervision during grace, not only after the unmanaged caller deadline");
    assert!(owner.0.try_wait().unwrap().is_none());
    assert_eq!(std::fs::read_to_string(pid_path()).unwrap(), owner.id().to_string());
}

#[tokio::test(start_paused = true)]
#[serial]
async fn supervisor_published_at_recovery_boundary_prevents_bootstrap() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    for recorded_owner in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        isolate(dir.path());
        let mut owner = LateMarkerOwner::spawn();
        assert!(owner.0.try_wait().unwrap().is_none());
        if recorded_owner {
            std::fs::write(pid_path(), owner.id().to_string()).unwrap();
        }
        let _hook = LateMarkerHookGuard::publish_at(
            SupervisorDiscoveryPoint::BeforeRecovery,
            marker_path(),
            owner.id(),
        );
        let spawn = || -> std::io::Result<std::process::Child> {
            Err(std::io::ErrorKind::NotFound.into())
        };
        let frame = request("stats()");
        let deadline = if recorded_owner { Duration::from_secs(12) } else { Duration::from_millis(250) };
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            khive_storage::scope_request_read_deadline(
                deadline,
                forward_or_spawn_with(&frame, &spawn),
            ),
        ).await;
        assert!(marker_path().exists(), "RECOVERY_BOUNDARY_REREADS_SUPERVISOR: publication seam reached");
        assert_late_supervisor_waited(result, "RECOVERY_BOUNDARY_REREADS_SUPERVISOR");
        assert!(owner.0.try_wait().unwrap().is_none());
        assert_eq!(pid_path().exists(), recorded_owner);
    }
}

#[tokio::test(start_paused = true)]
#[serial]
async fn supervisor_published_after_last_read_before_recovery_admission_prevents_spawn() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let mut owner = LateMarkerOwner::spawn();
    assert!(owner.0.try_wait().unwrap().is_none());
    let _hook = LateMarkerHookGuard::publish_at(
        SupervisorDiscoveryPoint::RecoveryAdmission,
        marker_path(),
        owner.id(),
    );
    let spawn_calls = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        spawn_calls.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let frame = request("stats()");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        khive_storage::scope_request_read_deadline(
            Duration::from_millis(250),
            forward_or_spawn_with(&frame, &spawn),
        ),
    )
    .await;
    assert!(
        marker_path().exists(),
        "RECOVERY_ADMISSION_LOCKED_REREAD: launcher publication seam reached"
    );
    assert_late_supervisor_waited(result, "RECOVERY_ADMISSION_LOCKED_REREAD");
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        0,
        "RECOVERY_ADMISSION_LOCKED_REREAD: no unmanaged child admitted"
    );
    assert!(!pid_path().exists());
    assert!(!socket_path().exists());
}

#[tokio::test(start_paused = true)]
#[serial]
async fn supervisor_republished_after_disappearance_before_admission_prevents_spawn() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    let mut owner = LateMarkerOwner::spawn();
    assert!(owner.0.try_wait().unwrap().is_none());
    let marker = marker_path();
    write_marker(&marker, "first.supervisor", owner.id(), Some(10));
    let republished = marker.clone();
    let owner_pid = owner.id();
    let _hook_cleanup = DiscoveryHookCleanup;
    {
        let mut hook = SUPERVISOR_DISCOVERY_HOOK.lock().unwrap();
        assert!(hook.is_none(), "isolated discovery hook");
        *hook = Some((SupervisorDiscoveryPoint::SupervisorWait, Box::new(move || {
            std::fs::remove_file(&marker).unwrap();
            *SUPERVISOR_DISCOVERY_HOOK.lock().unwrap() = Some((
                SupervisorDiscoveryPoint::RecoveryAdmission,
                Box::new(move || {
                    assert!(!republished.exists(), "first declaration was removed");
                    publish_marker_with_launcher_lock(
                        &republished,
                        "second.supervisor",
                        owner_pid,
                        Some(10),
                    );
                }),
            ));
        })));
    }
    let spawn_calls = AtomicUsize::new(0);
    let spawn = || -> std::io::Result<std::process::Child> {
        spawn_calls.fetch_add(1, Ordering::SeqCst);
        Err(std::io::ErrorKind::NotFound.into())
    };
    let frame = request("stats()");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        khive_storage::scope_request_read_deadline(
            Duration::from_millis(250),
            forward_or_spawn_with(&frame, &spawn),
        ),
    )
    .await
    .expect("REPUBLISH_LOCKED_REREAD: caller deadline must finish the request")
    .expect("REPUBLISH_LOCKED_REREAD: no local fallback")
    .expect_err("REPUBLISH_LOCKED_REREAD: no supervisor socket has been bound");
    assert_eq!(
        result.data.as_ref().and_then(|data| data.get("reason")),
        Some(&serde_json::json!("supervised_daemon_starting"))
    );
    assert!(result.message.contains("second.supervisor"));
    assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
    assert!(!pid_path().exists());
    assert!(!socket_path().exists());
    assert!(marker_path().exists());
}
