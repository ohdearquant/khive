#[tokio::test(flavor = "current_thread", start_paused = true)]
#[serial(background_tasks)]
async fn accepted_connection_is_counted_before_first_poll_and_drain_waits() {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let started_in_task = Arc::clone(&started);

    let handle = spawn_connection_task(Arc::clone(&active), async move {
        started_in_task.store(true, Ordering::Relaxed);
        let _ = release_rx.await;
    });

    assert_eq!(active.load(Ordering::Relaxed), 1);
    assert!(
        !started.load(Ordering::Relaxed),
        "the current-thread runtime must leave the spawned handler unpolled"
    );

    let drain_fut = drain(active.as_ref());
    tokio::pin!(drain_fut);
    let too_early =
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut drain_fut).await;
    assert!(
        too_early.is_err(),
        "drain must wait for a connection claimed before its task's first poll"
    );
    assert!(started.load(Ordering::Relaxed));

    release_tx.send(()).expect("handler still waiting");
    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("handler should finish promptly")
        .expect("handler should not panic");
    assert_eq!(active.load(Ordering::Relaxed), 0);
    tokio::time::timeout(std::time::Duration::from_secs(1), drain_fut)
        .await
        .expect("drain should finish once the handler releases its claim");
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_connection_releases_count_before_first_poll() {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_in_task = Arc::clone(&started);
    let handle = spawn_connection_task(Arc::clone(&active), async move {
        started_in_task.store(true, Ordering::Relaxed);
        std::future::pending::<()>().await;
    });

    assert_eq!(active.load(Ordering::Relaxed), 1);
    handle.abort();
    let error = handle.await.expect_err("aborted handler must be cancelled");
    assert!(error.is_cancelled());
    assert!(!started.load(Ordering::Relaxed));
    assert_eq!(active.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn panicked_connection_releases_count() {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = spawn_connection_task(Arc::clone(&active), async move {
        panic!("intentional connection-handler panic");
    });

    assert_eq!(active.load(Ordering::Relaxed), 1);
    let error = handle
        .await
        .expect_err("panicked handler must fail its join");
    assert!(error.is_panic());
    assert_eq!(active.load(Ordering::Relaxed), 0);
}

#[test]
fn connection_claim_releases_if_spawn_panics() {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(spawn_connection_task(Arc::clone(&active), async {}));
    }));

    assert!(result.is_err(), "tokio::spawn outside a runtime must panic");
    assert_eq!(active.load(Ordering::Relaxed), 0);
}

#[tokio::test]
#[serial(background_tasks)]
async fn drain_returns_promptly_with_no_accepted_connection() {
    let active = std::sync::atomic::AtomicUsize::new(0);
    tokio::time::timeout(std::time::Duration::from_secs(1), drain(&active))
        .await
        .expect("empty drain should return immediately");
}

#[test]
fn stopped_listener_is_closed_before_drain() {
    use std::process::{Command, Stdio};

    let dir = crate::test_process::private_tempdir_builder()
        .prefix("kh-drain-")
        .tempdir_in("/tmp")
        .expect("short isolated socket directory");
    let child_home = dir.path().join("home");
    std::fs::create_dir(&child_home).expect("empty daemon child HOME");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "daemon::tests::stopped_listener_is_closed_before_drain_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .envs(std::env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("KHIVE_")))
        .env("HOME", &child_home)
        .env("KHIVE_VOLUME_LOCK_DIR", dir.path().join("volume-locks"))
        .env_remove("LATTICE_MODEL_CACHE")
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_DRAIN_TEST_CHILD", "1")
        .env("KHIVE_SOCKET", dir.path().join("s"))
        .env("KHIVE_PID", dir.path().join("p"))
        .env("KHIVE_LOCK", dir.path().join("l"))
        .env("KHIVE_RECOVERER_LOCK", dir.path().join("r"))
        .env("KHIVE_DRAIN_TIMEOUT_SECS", "10")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn isolated daemon test");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let completed = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                break false;
            }
        }
    };
    let output = child.wait_with_output().expect("reap daemon test child");
    assert!(completed, "daemon test child did not finish: {output:?}");
    assert!(output.status.success(), "daemon test failed: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("STOPPED_LISTENER_DRAIN_VERIFIED"),
        "child must run the listener witness: {output:?}"
    );
    assert!(
        std::fs::read_dir(child_home).unwrap().next().is_none(),
        "daemon drain child must leave its private HOME empty"
    );
}

#[tokio::test]
#[ignore = "subprocess helper, invoked by stopped_listener_is_closed_before_drain"]
async fn stopped_listener_is_closed_before_drain_child() {
    assert_eq!(
        std::env::var("KHIVE_DRAIN_TEST_CHILD").expect("isolated child environment"),
        "1"
    );
    let _sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install child SIGTERM handler");
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let background = spawn_tracked_task(async move {
        release_rx.await.expect("release held drain task");
    });
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "drain-test".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_starts = Arc::clone(&starts);
    let callback_stopped = Arc::clone(&stopped);
    let boot_guard = Some(acquire_daemon_boot_guard().expect("boot guard"));
    let daemon = tokio::spawn(run_daemon_with_boot_guard_and_start(
        dispatcher,
        boot_guard,
        move |_| {
            use std::os::unix::fs::FileTypeExt;
            assert!(std::fs::metadata(socket_path())
                .unwrap()
                .file_type()
                .is_socket());
            assert_eq!(
                std::fs::read_to_string(pid_path()).unwrap(),
                std::process::id().to_string()
            );
            assert_eq!(
                callback_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                0
            );
            track_named_background_task("startup-lifecycle-test", async move {
                daemon_shutdown_token().cancelled().await;
                callback_stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        },
    ));
    let sock = socket_path();
    let mut stream = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = UnixStream::connect(&sock).await {
                break stream;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon must bind");
    let payload =
        serde_json::to_vec(&base_request_frame("drain-test")).expect("encode readiness request");
    write_frame(&mut stream, &payload)
        .await
        .expect("write readiness request");
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), read_frame(&mut stream))
        .await
        .expect("daemon must serve readiness request")
        .expect("read readiness response");
    let response: DaemonResponseFrame =
        serde_json::from_slice(&response).expect("decode readiness response");
    assert!(response.ok, "daemon readiness failed: {response:?}");
    assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(stream);

    // SAFETY: the isolated child signals only itself, after installing its handler.
    let rc = unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "signal isolated daemon child");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        daemon_shutdown_token().cancelled(),
    )
    .await
    .expect("daemon must begin shutdown");
    assert!(
        !daemon.is_finished(),
        "held background task must retain drain"
    );
    assert!(
        sock.exists(),
        "cleanup must not have removed the socket yet"
    );
    assert_eq!(
        std::fs::read_to_string(pid_path()).expect("draining daemon PID"),
        std::process::id().to_string()
    );

    // Cancellation is published after listener close but before drain.
    let late_connect = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        UnixStream::connect(&sock),
    )
    .await
    .expect("late connect must finish promptly");
    release_tx.send(()).expect("release daemon drain");
    background.await.expect("held background task must finish");
    tokio::time::timeout(std::time::Duration::from_secs(2), daemon)
        .await
        .expect("released daemon must finish shutdown")
        .expect("daemon task must not panic")
        .expect("daemon shutdown must succeed");
    let error = late_connect.expect_err("stopped listener must not queue new connections");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
    assert!(!sock.exists(), "owned socket must be removed after drain");
    assert!(
        !pid_path().exists(),
        "owned PID must be removed after drain"
    );
    assert!(
        stopped.load(std::sync::atomic::Ordering::SeqCst),
        "work started after ownership must finish inside daemon drain"
    );
    println!("STOPPED_LISTENER_DRAIN_VERIFIED");
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks)]
async fn graceful_drain_has_a_hard_upper_bound_with_stuck_work() {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task = spawn_connection_task(Arc::clone(&active), async {
        std::future::pending::<()>().await;
    });
    assert_eq!(active.load(Ordering::Relaxed), 1);
    let started = tokio::time::Instant::now();

    let drained = drain_with_timeout(&active, std::time::Duration::from_millis(250)).await;

    assert!(!drained, "stuck work must exhaust the drain bound");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(250)
            && started.elapsed() < std::time::Duration::from_millis(350),
        "graceful shutdown exceeded its configured bound: {:?}",
        started.elapsed()
    );
    finish_connection_tasks(vec![task], drained).await;
    assert_eq!(
        active.load(Ordering::Relaxed),
        0,
        "hard-bound escalation must abort, await, and release the handler"
    );
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks)]
async fn admitted_work_finishes_inside_drain_window_without_abort() {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let committed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let committed_in_task = Arc::clone(&committed);
    let task = spawn_connection_task(Arc::clone(&active), async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        committed_in_task.store(true, Ordering::SeqCst);
    });

    let drained = drain_with_timeout(&active, std::time::Duration::from_millis(250)).await;
    assert!(
        drained,
        "admitted work should finish inside the drain window"
    );
    finish_connection_tasks(vec![task], drained).await;
    assert!(committed.load(Ordering::SeqCst));
    assert_eq!(active.load(Ordering::Relaxed), 0);
}

// `drain()` must wait for tracked background tasks (e.g. memory.recall's
// serve-ledger append), not just in-flight connections, or a SIGTERM
// lands mid-flight with no log and no row.
//
// `#[serial(background_tasks)]`: this test reads/asserts on the
// process-wide `BACKGROUND_TASKS` static shared with the two counter
// tests below. Under default parallel execution one test's increment
// leaks into another's snapshot-then-assert window (reproduced: both
// counter tests failed together, passed with `--test-threads=1`).
// Serializing just this named group isolates them from each other
// without forcing the whole test binary (including unrelated
// `#[serial]` tests elsewhere in this crate) onto one thread.
#[tokio::test]
#[serial(background_tasks)]
async fn drain_waits_for_tracked_background_tasks_before_returning() {
    let active = std::sync::atomic::AtomicUsize::new(0);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    track_background_task(async move {
        let _ = rx.await;
    });
    assert!(
        background_task_count() >= 1,
        "track_background_task must make the in-flight task visible immediately"
    );

    let drain_fut = drain(&active);
    tokio::pin!(drain_fut);

    // Must NOT resolve while the tracked task is still pending.
    let too_early =
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut drain_fut).await;
    assert!(
        too_early.is_err(),
        "drain() must not return while a tracked background task is still running"
    );

    // Completing the task must let drain() proceed promptly.
    tx.send(())
        .expect("tracked task still awaiting the oneshot");
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), drain_fut).await;
    assert!(
        done.is_ok(),
        "drain() must return once the tracked background task finishes"
    );
}

#[tokio::test]
#[serial(background_tasks)]
async fn drain_waits_for_hydration_after_its_last_request_waiter_is_cancelled() {
    let before = background_task_count();
    let active = std::sync::atomic::AtomicUsize::new(0);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let store = Arc::new(DrainBlockingBlobStore {
        started: std::sync::Mutex::new(Some(started_tx)),
        release: Arc::clone(&release),
    });
    let hydrator = Arc::new(
        crate::BlobHydrator::new(
            store as Arc<dyn khive_storage::BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .expect("minimum hydration budget is valid"),
    );
    let content_ref =
        khive_storage::ContentRef::from_hex("a".repeat(64)).expect("fixture content ref");

    let request_hydrator = Arc::clone(&hydrator);
    let request = tokio::spawn(async move {
        request_hydrator
            .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
    });
    started_rx.await.expect("backend work must begin");
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(background_task_count(), before + 1);

    let draining = drain_with_timeout(&active, std::time::Duration::from_secs(5));
    tokio::pin!(draining);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut draining)
            .await
            .is_err(),
        "drain must remain pending while cancelled-request hydration still runs"
    );

    release.add_permits(1);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), draining)
            .await
            .expect("drain should finish after native hydration ends"),
        "hydration should finish inside the drain window"
    );
    assert_eq!(background_task_count(), before);
}

// See the `#[serial(background_tasks)]` note on
// `drain_waits_for_tracked_background_tasks_before_returning` above —
// this test shares the same process-wide `BACKGROUND_TASKS` static and
// races it (and the panic test below) under default parallelism.
#[tokio::test]
#[serial(background_tasks)]
async fn track_background_task_count_returns_to_zero_after_completion() {
    // Sanity check on the counter's own bookkeeping, independent of drain().
    let before = background_task_count();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    track_background_task(async move {
        let _ = rx.await;
    });
    assert_eq!(background_task_count(), before + 1);
    tx.send(()).expect("still awaiting");
    // Yield until the spawned task's decrement has actually run.
    for _ in 0..100 {
        if background_task_count() == before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(background_task_count(), before);
}

// See the `#[serial(background_tasks)]` note above — shares
// `BACKGROUND_TASKS` with the other two tests in this group.
#[tokio::test]
#[serial(background_tasks)]
async fn track_background_task_count_returns_to_baseline_after_panic() {
    // A panic inside the tracked future must still decrement the
    // counter (via BackgroundTaskGuard's Drop), not leak it forever.
    // `track_background_task` discards the spawned
    // task's `JoinHandle` (it is fire-and-forget by design — the caller
    // never awaits it), so this test does not await the panic directly;
    // tokio isolates the panic to the spawned task instead of aborting
    // the process, and we observe the recovery purely through the
    // shared counter returning to baseline after the guard's `Drop`
    // fires during that task's unwind.
    let before = background_task_count();

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    track_background_task(async move {
        let _ = rx.await;
        panic!("intentional panic to exercise the Drop-guard decrement path");
    });
    assert_eq!(background_task_count(), before + 1);

    tx.send(()).expect("still awaiting");
    for _ in 0..100 {
        if background_task_count() == before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        background_task_count(),
        before,
        "background task counter must return to baseline after the tracked future panics"
    );
}

// ── active background phase names (ADR-103) ──────────────────────────

// `#[serial(active_phases)]`: these tests write the process-wide
// `ACTIVE_PHASES` static. The demand-retirement tests below read it through
// `idle_retirement_blockers`, where any registered phase is an idle blocker,
// so the demand-retirement tests that call it also take `active_phases`.
// A phase held here while one of them expects retirement turns it red.
#[test]
#[serial(active_phases)]
fn register_active_phase_appears_and_disappears_with_the_guard() {
    assert!(
        !active_phase_names().contains(&"adr103_test_phase".to_string()),
        "must start absent (leaked from a prior failed run would poison this test)"
    );

    let guard = register_active_phase("adr103_test_phase");
    assert!(active_phase_names().contains(&"adr103_test_phase".to_string()));

    drop(guard);
    assert!(
        !active_phase_names().contains(&"adr103_test_phase".to_string()),
        "the phase name must drop out of the gauge once its guard is dropped"
    );
}

#[test]
#[serial(active_phases)]
fn register_active_phase_counts_concurrent_occurrences_of_the_same_name() {
    let first = register_active_phase("adr103_concurrent_phase");
    let second = register_active_phase("adr103_concurrent_phase");
    assert!(active_phase_names().contains(&"adr103_concurrent_phase".to_string()));

    drop(first);
    assert!(
        active_phase_names().contains(&"adr103_concurrent_phase".to_string()),
        "one of two concurrent occurrences ending must not remove the name early"
    );

    drop(second);
    assert!(
        !active_phase_names().contains(&"adr103_concurrent_phase".to_string()),
        "the name must be removed only once every concurrent occurrence has ended"
    );
}
