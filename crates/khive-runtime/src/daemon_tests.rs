include!("daemon/plan_tests.rs");
mod shutdown_signals {
    include!("daemon/shutdown_signal_tests.rs");
}
#[path = "daemon/tests/connection_limit_tests.rs"]
mod connection_limit_tests;
use super::*;
use serial_test::serial;

#[test]
fn lexical_timeout_detail_hides_marker_from_old_clients_without_changing_frame_fit() {
    let public = serde_json::json!({
        "results": [{"ok": true, "tool": "knowledge.search", "result": "| name |\n|---|\n| first |\n"}],
        "summary": {"total": 1, "succeeded": 1, "failed": 0}
    });
    let public_raw = public.to_string();
    let mut marked = public;
    marked[DAEMON_LEXICAL_TIMEOUT_MARKER] = serde_json::json!(true);
    let marked_raw = marked.to_string();
    let (result, detail) = take_daemon_lexical_timeout_marker(marked_raw.clone());
    assert_eq!(result, public_raw);
    assert_eq!(detail, Some(serde_json::json!({"lexical_timeout": true})));

    let frame = |result, error_detail| DaemonResponseFrame {
        ok: true,
        result: Some(result),
        error: None,
        error_detail,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some("test".to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: Some(u64::MAX),
    };
    let internal_len = serde_json::to_vec(&frame(marked_raw, None)).unwrap().len();
    let sent = frame(result, detail);
    assert_eq!(sent.result.as_deref(), Some(public_raw.as_str()));
    assert!(!sent
        .result
        .as_deref()
        .unwrap()
        .contains(DAEMON_LEXICAL_TIMEOUT_MARKER));
    assert_eq!(serde_json::to_vec(&sent).unwrap().len(), internal_len);

    let untouched = " {\"results\":[],\"summary\":{}} ".to_string();
    assert_eq!(
        take_daemon_lexical_timeout_marker(untouched.clone()),
        (untouched, None)
    );
}

#[tokio::test]
async fn incomplete_initial_frames_release_the_connection_deadline() {
    for prefix in [&[][..], &[0, 0][..], &[0, 0, 0, 5][..]] {
        let (mut peer, mut server) = tokio::io::duplex(64);
        peer.write_all(prefix).await.expect("send partial frame");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(10);
        let error = read_initial_frame(&mut server, deadline)
            .await
            .expect_err("an idle peer cannot hold a daemon connection indefinitely");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    let (mut peer, mut server) = tokio::io::duplex(64);
    write_frame(&mut peer, b"{}")
        .await
        .expect("send full frame");
    assert_eq!(
        read_initial_frame(
            &mut server,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .expect("complete frame remains readable"),
        b"{}"
    );
}

#[test]
fn repeated_accept_failures_back_off_and_cap_at_one_second() {
    let mut previous = None;
    for expected_ms in [10, 20, 40, 80, 160, 320, 640, 1000, 1000] {
        let next = next_accept_error_backoff(previous);
        assert_eq!(next.as_millis(), expected_ms);
        previous = Some(next);
    }
    assert_eq!(next_accept_error_backoff(None).as_millis(), 10);
}

#[derive(Debug)]
struct DrainBlockingBlobStore {
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl khive_storage::BlobStore for DrainBlockingBlobStore {
    async fn put(
        &self,
        _bytes: Vec<u8>,
    ) -> khive_storage::StorageResult<khive_storage::ContentRef> {
        panic!("put is not used by the hydration drain test")
    }

    async fn get_bounded_verified(
        &self,
        _content_ref: &khive_storage::ContentRef,
        _max_bytes: u64,
    ) -> khive_storage::StorageResult<Vec<u8>> {
        if let Some(started) = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = started.send(());
        }
        self.release
            .clone()
            .acquire_owned()
            .await
            .expect("test release semaphore remains open")
            .forget();
        Ok(b"late result".to_vec())
    }

    async fn exists(
        &self,
        _content_ref: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<bool> {
        panic!("exists is not used by the hydration drain test")
    }

    async fn size(
        &self,
        _content_ref: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<Option<u64>> {
        panic!("size is not used by the hydration drain test")
    }

    async fn delete(
        &self,
        _content_ref: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<bool> {
        panic!("delete is not used by the hydration drain test")
    }
}

struct AppendCompletionEventStore {
    inner: Arc<dyn khive_storage::EventStore>,
    first_append: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[async_trait]
impl khive_storage::EventStore for AppendCompletionEventStore {
    async fn append_event(&self, event: khive_storage::Event) -> khive_storage::StorageResult<()> {
        self.inner.append_event(event).await?;
        if let Some(completed) = self
            .first_append
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = completed.send(());
        }
        Ok(())
    }

    async fn append_events(
        &self,
        events: Vec<khive_storage::Event>,
    ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
        self.inner.append_events(events).await
    }

    async fn get_event(
        &self,
        id: uuid::Uuid,
    ) -> khive_storage::StorageResult<Option<khive_storage::Event>> {
        self.inner.get_event(id).await
    }

    async fn query_events(
        &self,
        filter: khive_storage::EventFilter,
        page: khive_storage::PageRequest,
    ) -> khive_storage::StorageResult<khive_storage::Page<khive_storage::Event>> {
        self.inner.query_events(filter, page).await
    }

    async fn count_events(
        &self,
        filter: khive_storage::EventFilter,
    ) -> khive_storage::StorageResult<u64> {
        self.inner.count_events(filter).await
    }
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn secondary_only_checkpoint_topology_emits_lifecycle_outcome() {
    let main_backend = khive_db::StorageBackend::memory().expect("in-memory main backend");
    let inner_event_store = main_backend.events().expect("main event store");
    let (first_append_tx, first_append_rx) = tokio::sync::oneshot::channel();
    let event_store: Arc<dyn khive_storage::EventStore> = Arc::new(AppendCompletionEventStore {
        inner: inner_event_store,
        first_append: std::sync::Mutex::new(Some(first_append_tx)),
    });
    let secondary_dir = tempfile::tempdir().expect("secondary tempdir");
    let secondary_backend =
        khive_db::StorageBackend::sqlite_for_test(secondary_dir.path().join("secondary.db"))
            .expect("file-backed secondary backend");

    let mut tasks = checkpoint_task_specs(
        None,
        vec![secondary_backend.pool_arc()],
        Some(Arc::clone(&event_store)),
        "local".to_string(),
    );
    assert_eq!(tasks.len(), 1);
    let task = tasks.pop().expect("one secondary checkpoint task");
    assert!(!task.is_main, "the only checkpoint task must be secondary");
    assert!(
        task.lifecycle_owner.is_some(),
        "the secondary task must own lifecycle emission when no main task exists"
    );

    let config = CheckpointConfig {
        interval: std::time::Duration::from_millis(10),
        warn_pages: 0,
        ..CheckpointConfig::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(run_checkpoint_task(
        task.pool,
        config,
        task.lifecycle_owner,
        shutdown_rx,
        task.is_main,
    ));

    // The scheduler intentionally aborts its bounded append worker during
    // shutdown. Wait on the append's completion edge before requesting
    // shutdown, so the observation cannot race that deliberate abort.
    tokio::time::timeout(std::time::Duration::from_secs(10), first_append_rx)
        .await
        .expect("secondary checkpoint owner did not complete an append within 10s")
        .expect("checkpoint lifecycle append completion sender dropped");

    let events = event_store
        .query_events(
            khive_storage::EventFilter::default(),
            khive_storage::PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .expect("query lifecycle events");

    shutdown_tx.send(()).expect("send checkpoint shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("checkpoint task should exit within 1s")
        .expect("checkpoint task panicked");
    assert!(
        !events.items.is_empty()
            && events
                .items
                .iter()
                .all(|event| event.kind == khive_types::EventKind::CheckpointOutcomeRecorded),
        "the designated secondary owner must emit CheckpointOutcomeRecorded"
    );

    let file_main_dir = tempfile::tempdir().expect("file-backed main tempdir");
    let file_main = khive_db::StorageBackend::sqlite_for_test(file_main_dir.path().join("main.db"))
        .expect("file-backed main backend");
    let tasks = checkpoint_task_specs(
        Some(file_main.pool_arc()),
        vec![secondary_backend.pool_arc()],
        Some(event_store),
        "local".to_string(),
    );
    assert!(tasks[0].is_main && tasks[0].lifecycle_owner.is_some());
    assert!(!tasks[1].is_main && tasks[1].lifecycle_owner.is_none());
}

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
        let dir = tempfile::tempdir().expect("tempdir");
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

    let dir = tempfile::tempdir().expect("tempdir");
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

    let dir = tempfile::tempdir().expect("tempdir");
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

    let dir = tempfile::Builder::new()
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

// `#[serial(active_phases)]`: these tests read/assert on the process-wide
// `ACTIVE_PHASES` static. No other test in this crate touches it today,
// but the group mirrors the `background_tasks` precedent above so a
// future addition does not silently reintroduce the same interleaving
// hazard that motivated it there.
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

// ── metrics-only frame (load/perf harness read-surface) ────────────────

/// Minimal `DaemonDispatch` for the metrics tests: `dispatch` just counts
/// how many times it was called (so tests can assert the ops path was
/// never reached) and `pool_for_checkpoint` returns whatever pool the
/// test wired in (or `None`, matching an in-memory/poolless dispatcher).
#[derive(Clone)]
struct MockDispatch {
    namespace: String,
    config_id: String,
    dispatch_calls: Arc<std::sync::atomic::AtomicUsize>,
    pool: Option<Arc<ConnectionPool>>,
    /// When `Some(msg)`, `dispatch` returns `Err(msg)` instead of the
    /// default `Ok("{}")` — lets a test drive `handle_conn`'s real
    /// dispatch-error arm (khive#948 request_id echo coverage).
    dispatch_err: Option<String>,
}

#[derive(Clone)]
struct CancellationAwareDispatch {
    started: Arc<tokio::sync::Notify>,
    cancellation_observed: Arc<std::sync::atomic::AtomicBool>,
    count_sql: Option<Arc<dyn khive_storage::SqlAccess>>,
}

#[async_trait]
impl DaemonDispatch for CancellationAwareDispatch {
    fn plan(&self, ops: &str) -> String {
        khive_request::plan_request(ops, &Default::default()).to_string()
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<RequestIdentity>,
    ) -> Result<String, String> {
        self.started.notify_one();
        if let Some(sql) = &self.count_sql {
            let mut reader = sql.reader().await.map_err(|error| error.to_string())?;
            let result = reader.query_scalar(khive_storage::SqlStatement {
                    sql: "SELECT COUNT(*) FROM events WHERE namespace = ?1 AND verb LIKE 'knowledge.%'".into(),
                    params: vec![khive_storage::SqlValue::Text("local".into())],
                    label: Some("knowledge.stats.event_count".into()),
                }).await;
            self.cancellation_observed.store(
                matches!(
                    result,
                    Err(khive_storage::error::StorageError::Timeout { .. })
                ),
                std::sync::atomic::Ordering::SeqCst,
            );
            return result
                .map(|value| format!("{value:?}"))
                .map_err(|error| error.to_string());
        }
        khive_storage::wait_for_request_read_cancellation().await;
        self.cancellation_observed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok("{}".to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        "local"
    }

    fn config_id(&self) -> &str {
        "disconnect-test"
    }
}

mod demand_retirement_tests {
    use super::*;
    use khive_storage::SqlAccess;

    pub(super) fn dispatcher(pool: Option<Arc<ConnectionPool>>) -> MockDispatch {
        MockDispatch {
            namespace: "local".to_owned(),
            config_id: "idle-test".to_owned(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool,
            dispatch_err: None,
        }
    }

    pub(super) fn lifecycle(mode: DaemonLifetime) -> Arc<DaemonLifecycle> {
        Arc::new(DaemonLifecycle::new(
            DaemonOptions {
                lifetime: mode,
                idle_interval: std::time::Duration::from_secs(1),
            },
            DaemonStartupReport::default(),
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_cleanup_resets_idle_and_admission_is_one_way() {
        let state = lifecycle(DaemonLifetime::Demand);
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(
            !state.try_idle(Vec::new),
            "readiness must precede the idle clock"
        );
        state.ready();
        let request = state.admit().unwrap();
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(
            !state.try_idle(Vec::new),
            "admitted work must prevent retirement"
        );
        drop(request);
        assert!(
            !state.try_idle(Vec::new),
            "cleanup starts a fresh idle interval"
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(state.try_idle(Vec::new));
        assert!(
            state.admit().is_none(),
            "draining must refuse before dispatch"
        );
        assert!(!state.try_idle(Vec::new), "retirement cannot be repeated");
        assert_eq!(
            state.snapshot().shutdown_reason,
            Some(DaemonShutdownReason::Idle)
        );
        state.stopped();
        assert!(state.admit().is_none());
    }

    #[test]
    fn concurrent_admission_and_idle_decision_choose_one_winner() {
        for _ in 0..16 {
            let state = Arc::new(DaemonLifecycle::new(
                DaemonOptions {
                    lifetime: DaemonLifetime::Demand,
                    idle_interval: std::time::Duration::from_nanos(1),
                },
                DaemonStartupReport::default(),
            ));
            state.ready();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let admitting_state = Arc::clone(&state);
            let admitting_barrier = Arc::clone(&barrier);
            let admission = std::thread::spawn(move || {
                admitting_barrier.wait();
                admitting_state.admit()
            });
            let retiring_state = Arc::clone(&state);
            let retirement = std::thread::spawn(move || {
                barrier.wait();
                retiring_state.try_idle(Vec::new)
            });
            let admitted = admission.join().unwrap();
            let retired = retirement.join().unwrap();
            assert_ne!(
                admitted.is_some(),
                retired,
                "request admission and voluntary retirement cannot both win"
            );
            if retired {
                assert!(state.admit().is_none());
            }
            drop(admitted);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn named_service_obligations_and_unknown_resources_are_ineligible() {
        let state = Arc::new(DaemonLifecycle::new(
            DaemonOptions {
                lifetime: DaemonLifetime::Demand,
                idle_interval: std::time::Duration::from_secs(1),
            },
            DaemonStartupReport {
                skipped_components: vec!["schedule-tick".to_owned()],
                idle_ineligible_reasons: vec!["unclassified_component:external-service".to_owned()],
            },
        ));
        state.ready();
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(!state.try_idle(Vec::new));
        assert_eq!(
            state.snapshot().idle_ineligible_reasons,
            vec!["unclassified_component:external-service"]
        );
        let unknown = CancellationAwareDispatch {
            started: Arc::new(tokio::sync::Notify::new()),
            cancellation_observed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            count_sql: None,
        };
        let clean = lifecycle(DaemonLifetime::Demand);
        clean.ready();
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(!clean.try_idle(|| unknown.idle_retirement_blockers()));
        assert_eq!(
            clean.snapshot().idle_blockers,
            vec!["dispatcher_resource_inventory_unknown"]
        );
    }

    #[tokio::test(start_paused = true)]
    #[serial(background_tasks, tx_registry)]
    async fn retained_raw_sql_writer_blocks_actual_idle_wait_and_persistent_stays() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(dir.path().join("retained.db")),
                write_queue_enabled: Some(false),
                write_routing_strict: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let bridge = khive_db::SqlBridge::new(Arc::clone(&pool), true);
        let writer = bridge.writer().await.unwrap();
        assert!(
            khive_storage::tx_registry::snapshot().is_empty(),
            "this hold must be autocommit, not an open transaction"
        );
        let d = dispatcher(Some(Arc::clone(&pool)));
        let demand = lifecycle(DaemonLifetime::Demand);
        let persistent = lifecycle(DaemonLifetime::Persistent);
        demand.ready();
        persistent.ready();
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        let idle = wait_for_idle(&d, &demand);
        tokio::pin!(idle);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut idle)
                .await
                .is_err(),
            "a genuine retained writer handle must prevent the actual idle arm"
        );
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(150),
            wait_for_idle(&d, &persistent)
        )
        .await
        .is_err());
        drop(writer);
        assert_eq!(pool.retirement_writer_holds(), 0);
        tokio::time::timeout(std::time::Duration::from_secs(2), idle)
            .await
            .unwrap();
        assert_eq!(demand.snapshot().phase, DaemonLifecyclePhase::Draining);
        assert_eq!(persistent.snapshot().phase, DaemonLifecyclePhase::Serving);
        let pooled = lifecycle(DaemonLifetime::Demand);
        pooled.ready();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let pooled_guard = pool.writer().unwrap();
        assert!(
            !pooled.try_idle(|| idle_retirement_blockers(&d)),
            "pooled writer hold must block retirement"
        );
        drop(pooled_guard);
        assert!(pooled.try_idle(|| idle_retirement_blockers(&d)));
    }

    #[tokio::test(start_paused = true)]
    #[serial(background_tasks, tx_registry)]
    async fn persistent_idle_wait_never_retires_after_writer_release() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(dir.path().join("persistent.db")),
                write_queue_enabled: Some(false),
                write_routing_strict: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let bridge = khive_db::SqlBridge::new(Arc::clone(&pool), true);
        let held = bridge.writer().await.unwrap();
        let d = dispatcher(Some(pool));
        let state = lifecycle(DaemonLifetime::Persistent);
        state.ready();
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_for_idle(&d, &state)
        )
        .await
        .is_err());
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), wait_for_idle(&d, &state))
                .await
                .is_err()
        );
        assert_eq!(state.snapshot().phase, DaemonLifecyclePhase::Serving);
    }

    #[tokio::test(start_paused = true)]
    #[serial(background_tasks, tx_registry)]
    async fn explicit_sql_reader_transaction_blocks_retirement_without_writer_hold() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(dir.path().join("reader.db")),
                write_queue_enabled: Some(false),
                write_routing_strict: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let bridge = khive_db::SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();
        reader
            .query_all(khive_storage::SqlStatement {
                sql: "BEGIN DEFERRED".to_owned(),
                params: vec![],
                label: Some("idle-reader".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(pool.retirement_writer_holds(), 0);
        let d = dispatcher(Some(pool));
        let state = lifecycle(DaemonLifetime::Demand);
        state.ready();
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(!state.try_idle(|| idle_retirement_blockers(&d)));
        assert!(state
            .snapshot()
            .idle_blockers
            .contains(&"open_sql_transaction".to_owned()));
        drop(reader);
        assert!(state.try_idle(|| idle_retirement_blockers(&d)));
    }

    #[tokio::test(start_paused = true)]
    #[serial(background_tasks, tx_registry)]
    async fn unsettled_named_worker_blocks_idle_without_resetting_clock() {
        let (release, pending) = tokio::sync::oneshot::channel::<()>();
        let task = spawn_named_tracked_task("idle-test-worker", async move {
            pending.await.unwrap();
        });
        let state = lifecycle(DaemonLifetime::Demand);
        state.ready();
        let d = dispatcher(None);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert!(!state.try_idle(|| idle_retirement_blockers(&d)));
        assert!(state
            .snapshot()
            .idle_blockers
            .contains(&"unsettled_worker:idle-test-worker".to_owned()));
        release.send(()).unwrap();
        task.await.unwrap();
        assert!(
            state.try_idle(|| idle_retirement_blockers(&d)),
            "maintenance completion must not reset ordinary activity"
        );
    }

    #[tokio::test(start_paused = true)]
    #[serial(background_tasks)]
    async fn voluntary_drain_retains_pending_work_past_deadline() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release, pending) = tokio::sync::oneshot::channel::<()>();
        let task = spawn_connection_task(Arc::clone(&active), async move {
            pending.await.unwrap();
        });
        let drain = drain_for_idle(&active, std::time::Duration::from_millis(10));
        tokio::pin!(drain);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut drain)
                .await
                .is_err(),
            "voluntary timeout must retain admitted work"
        );
        assert!(!task.is_finished());
        release.send(()).unwrap();
        task.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), drain)
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_response_transport_is_bounded() {
        let (mut writer, _held_reader) = tokio::io::duplex(1);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(35),
            write_response_frame(&mut writer, b"bounded response"),
        )
        .await
        .expect("the production response bound must fire before the fixture ceiling")
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn draining_handler_refuses_before_dispatch() {
        let d = dispatcher(None);
        let calls = Arc::clone(&d.dispatch_calls);
        let state = lifecycle(DaemonLifetime::Demand);
        state.ready();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert!(state.try_idle(Vec::new));
        // The lifecycle transition is already fixed. Real Unix socket
        // readiness must not race the paused clock's automatic timeout jump.
        tokio::time::resume();
        let (mut client, server) = UnixStream::pair().unwrap();
        let handle = tokio::spawn(handle_conn_with_lifecycle(
            server,
            d,
            None,
            tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
            Some(state),
        ));
        let mut frame = base_request_frame("idle-test");
        frame.ops = "stats()".to_owned();
        write_frame(&mut client, &serde_json::to_vec(&frame).unwrap())
            .await
            .unwrap();
        let refusal: DaemonResponseFrame =
            serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
        assert!(!refusal.ok);
        assert_eq!(refusal.error_detail.unwrap()["code"], "daemon_draining");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        handle.await.unwrap();
    }

    #[test]
    fn lifecycle_metrics_are_additive_and_generation_is_stable() {
        let state = lifecycle(DaemonLifetime::Demand);
        let generation = state.snapshot().instance_generation;
        let metrics = MetricsSnapshot {
            lifecycle: Some(state.snapshot()),
            ..Default::default()
        };
        let decoded: MetricsSnapshot =
            serde_json::from_value(serde_json::to_value(metrics).unwrap()).unwrap();
        assert_eq!(decoded.lifecycle.unwrap().instance_generation, generation);
        let old = serde_json::to_value(MetricsSnapshot::default()).unwrap();
        assert!(old.get("lifecycle").is_none());
        assert!(serde_json::from_value::<MetricsSnapshot>(old)
            .unwrap()
            .lifecycle
            .is_none());
    }
}

#[async_trait]
impl DaemonDispatch for MockDispatch {
    fn idle_retirement_blockers(&self) -> Vec<String> {
        self.pool
            .as_ref()
            .filter(|pool| pool.retirement_writer_holds() != 0)
            .map(|_| vec!["test_backend:held_writer".to_owned()])
            .unwrap_or_default()
    }

    fn plan(&self, ops: &str) -> String {
        khive_request::plan_request(ops, &Default::default()).to_string()
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<RequestIdentity>,
    ) -> Result<String, String> {
        self.dispatch_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match &self.dispatch_err {
            Some(msg) => Err(msg.clone()),
            None => Ok("{}".to_string()),
        }
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn config_id(&self) -> &str {
        &self.config_id
    }

    fn pool_for_checkpoint(&self) -> Option<Arc<ConnectionPool>> {
        self.pool.clone()
    }
}

fn base_request_frame(config_id: &str) -> DaemonRequestFrame {
    DaemonRequestFrame {
        plan: false,
        ops: String::new(),
        presentation: None,
        presentation_per_op: None,
        namespace: "local".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    }
}

/// Drive `handle_conn` over an in-process `UnixStream::pair()` (no real
/// socket file needed) and decode the response frame it writes back.
async fn round_trip<D: DaemonDispatch>(
    dispatcher: D,
    req: &DaemonRequestFrame,
) -> DaemonResponseFrame {
    let (mut client, server) = UnixStream::pair().expect("unix stream pair");
    let payload = serde_json::to_vec(req).expect("encode request frame");
    let handle = tokio::spawn(async move {
        handle_conn(server, dispatcher).await;
    });
    write_frame(&mut client, &payload)
        .await
        .expect("write request frame");
    let raw = read_frame(&mut client).await.expect("read response frame");
    handle.await.expect("handle_conn task panicked");
    serde_json::from_slice(&raw).expect("decode response frame")
}

#[tokio::test]
async fn expired_accepted_deadline_refuses_even_buffered_complete_frame() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".into(),
        config_id: "expired-accept-test".into(),
        dispatch_calls: Arc::clone(&calls),
        pool: None,
        dispatch_err: None,
    };
    let (mut client, server) = UnixStream::pair().expect("unix stream pair");
    let frame = base_request_frame("expired-accept-test");
    write_frame(&mut client, &serde_json::to_vec(&frame).unwrap())
        .await
        .expect("buffer complete frame before handler starts");
    // Model a task first polled after its acceptance-time deadline. Tokio
    // polls a ready frame before its timer, so timeout_at alone would
    // wrongly dispatch this already-buffered request.
    let accepted_deadline = tokio::time::Instant::now() - std::time::Duration::from_secs(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        handle_conn_with_shutdown(server, dispatcher, None, accepted_deadline),
    )
    .await
    .expect("expired accepted deadline must not start a fresh read window");
    let mut byte = [0u8; 1];
    match client.read(&mut byte).await {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("expected closed socket, got {other:?}"),
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Supplies the entire frame without registering readiness or yielding.
/// `timeout_at` must not be allowed to accept this ready first poll after
/// the connection's acceptance-time deadline has already passed.
struct ReadyFrameReader {
    frame: Vec<u8>,
    offset: usize,
    polls: usize,
}

impl tokio::io::AsyncRead for ReadyFrameReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let reader = self.get_mut();
        reader.polls += 1;
        let remaining = &reader.frame[reader.offset..];
        let count = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..count]);
        reader.offset += count;
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn expired_accepted_deadline_refuses_a_frame_ready_on_first_poll() {
    let mut reader = ReadyFrameReader {
        frame: [2_u32.to_be_bytes().as_slice(), b"{}"].concat(),
        offset: 0,
        polls: 0,
    };
    let accepted_deadline = tokio::time::Instant::now() - std::time::Duration::from_secs(1);
    let error = read_initial_frame(&mut reader, accepted_deadline)
        .await
        .expect_err("a fully ready frame must not outlive its acceptance deadline");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(reader.polls, 0, "an expired frame must not be polled");
}

/// #2230 review (Medium): duplicate-daemon detection must not treat any
/// accepting Unix listener as khived. A real khived (`handle_conn` behind
/// a bound socket) must still be recognized by the protocol probe.
#[tokio::test]
async fn socket_speaks_khived_protocol_accepts_a_real_khived() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("real.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind real listener");
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "probe-test".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let accept_task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            handle_conn(stream, dispatcher).await;
        }
    });

    assert!(
        socket_speaks_khived_protocol(&sock_path, "probe-test").await,
        "a real khived answering the probe_only frame with a matching config_id must be recognized"
    );

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), accept_task).await;
}

include!("daemon/probe_listener_tests.rs");

/// Regression (#2230): a well-formed [`DaemonResponseFrame`]
/// that is not the unambiguous probe-ack sentinel — e.g. one reporting a
/// `config_mismatch` for a *different* config_id, exactly what a live
/// khived serving another store would send back — must not be treated as
/// the same live, identity-matching duplicate. Before this fix, any
/// frame that merely deserialized was accepted, so this response would
/// have been misclassified as "alive" and refused a legitimate boot.
#[tokio::test]
async fn socket_speaks_khived_protocol_rejects_a_non_ack_or_mismatched_response() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("mismatched.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind fake listener");
    let accept_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _raw = read_frame(&mut stream).await.expect("read probe frame");
            let resp = DaemonResponseFrame {
                ok: false,
                result: None,
                error: None,
                namespace_mismatch: false,
                config_mismatch: true,
                served_config_id: Some("someone-elses-config".to_string()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                error_detail: None,
                metrics: None,
                request_id: None,
            };
            let payload = serde_json::to_vec(&resp).expect("encode response");
            write_frame(&mut stream, &payload)
                .await
                .expect("write response");
        }
    });

    let speaks = socket_speaks_khived_protocol(&sock_path, "expected-config").await;
    assert!(
        !speaks,
        "a well-formed but non-ack / identity-mismatched response must not be treated as \
             the same live khived"
    );

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), accept_task).await;
}

/// Regression (#2230): the daemon's `metrics_only` arm answers with
/// `ok=true, result=None, error=None`, every mismatch flag false, the
/// current protocol version, and a matching `served_config_id` — the
/// exact same shape the probe-ack arm produces, differing only in
/// carrying `metrics: Some(...)`. A well-formed metrics snapshot
/// response must not be misread as a probe acknowledgement; otherwise a
/// client whose only interaction with the socket happened to be a
/// metrics poll would be classified as the same live, identity-matching
/// khived. This response carries `request_id: None`, so it isolates the
/// `metrics.is_none()` conjunct — see the sibling test below for the
/// `request_id.is_none()` conjunct.
#[tokio::test]
async fn socket_speaks_khived_protocol_rejects_a_metrics_only_response() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("metrics-only.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind fake listener");
    let accept_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _raw = read_frame(&mut stream).await.expect("read probe frame");
            let resp = DaemonResponseFrame {
                ok: true,
                result: None,
                error: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some("expected-config".to_string()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                error_detail: None,
                metrics: Some(MetricsSnapshot::default()),
                request_id: None,
            };
            let payload = serde_json::to_vec(&resp).expect("encode response");
            write_frame(&mut stream, &payload)
                .await
                .expect("write response");
        }
    });

    let speaks = socket_speaks_khived_protocol(&sock_path, "expected-config").await;
    assert!(
        !speaks,
        "an otherwise-matching response carrying a metrics snapshot must not be treated as \
             a probe acknowledgement"
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), accept_task)
        .await
        .expect("fake listener accept task timed out")
        .expect("fake listener accept task panicked");
}

/// Regression (#2230): sibling of the metrics-only test above, isolating
/// the `request_id.is_none()` conjunct. A response with `metrics: None`
/// but an echoed `request_id: Some(_)` is otherwise identical to a probe
/// acknowledgement and must not be misread as one — a probe frame never
/// sets `request_id`, so an echo of one is proof the peer answered a
/// different, non-probe request.
#[tokio::test]
async fn socket_speaks_khived_protocol_rejects_a_response_with_request_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("request-id.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind fake listener");
    let accept_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _raw = read_frame(&mut stream).await.expect("read probe frame");
            let resp = DaemonResponseFrame {
                ok: true,
                result: None,
                error: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some("expected-config".to_string()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                error_detail: None,
                metrics: None,
                request_id: Some(42),
            };
            let payload = serde_json::to_vec(&resp).expect("encode response");
            write_frame(&mut stream, &payload)
                .await
                .expect("write response");
        }
    });

    let speaks = socket_speaks_khived_protocol(&sock_path, "expected-config").await;
    assert!(
        !speaks,
        "an otherwise-matching response carrying an echoed request_id must not be treated \
             as a probe acknowledgement"
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), accept_task)
        .await
        .expect("fake listener accept task timed out")
        .expect("fake listener accept task panicked");
}

#[derive(Clone)]
struct DetailedDispatch {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    detail: serde_json::Value,
}

#[async_trait]
impl DaemonDispatch for DetailedDispatch {
    fn plan(&self, ops: &str) -> String {
        khive_request::plan_request(ops, &Default::default()).to_string()
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<RequestIdentity>,
    ) -> Result<String, String> {
        panic!("the daemon must use the detailed dispatch seam");
    }

    async fn dispatch_with_error_detail(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<RequestIdentity>,
    ) -> Result<String, DaemonDispatchError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(DaemonDispatchError::new(
            "audit failed",
            Some(self.detail.clone()),
        ))
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        "local"
    }

    fn config_id(&self) -> &str {
        "disposition-test"
    }
}

#[tokio::test]
async fn disposition_detail_survives_daemon_framing_and_legacy_v4_decoder() {
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    struct LegacyV4Response {
        ok: bool,
        result: Option<String>,
        error: Option<String>,
        namespace_mismatch: bool,
        #[serde(default)]
        config_mismatch: bool,
        #[serde(default)]
        served_config_id: Option<String>,
        #[serde(default)]
        version_mismatch: bool,
        #[serde(default)]
        daemon_protocol_version: u32,
        #[serde(default)]
        metrics: Option<MetricsSnapshot>,
        #[serde(default)]
        request_id: Option<u64>,
    }

    let detail = serde_json::json!({
        "kind": "obligation",
        "code": "store_failure",
        "message": "audit failed",
        "domain_disposition": "committed",
        "domain_result": { "id": "persisted-row" },
    });
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response = round_trip(
        DetailedDispatch {
            calls: Arc::clone(&calls),
            detail: detail.clone(),
        },
        &base_request_frame("disposition-test"),
    )
    .await;
    assert_eq!(response.error_detail.as_ref(), Some(&detail));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let encoded = serde_json::to_vec(&response).expect("serialize detailed response");
    let legacy: LegacyV4Response = serde_json::from_slice(&encoded).expect("legacy v4 decode");
    assert!(!legacy.ok);
    assert_eq!(legacy.error.as_deref(), Some("audit failed"));
    assert_eq!(legacy.daemon_protocol_version, PROTOCOL_VERSION);
}

#[tokio::test]
async fn disposition_legacy_dispatch_error_is_unknown_and_success_has_no_detail() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "disposition-test".to_string(),
        dispatch_calls: Arc::clone(&calls),
        pool: None,
        dispatch_err: Some("legacy failure".to_string()),
    };
    let request = base_request_frame("disposition-test");
    let failure = round_trip(dispatcher.clone(), &request).await;
    assert_eq!(
        failure.error_detail.as_ref().unwrap()["domain_disposition"],
        "unknown"
    );
    assert_eq!(failure.error.as_deref(), Some("legacy failure"));
    let success = round_trip(
        MockDispatch {
            dispatch_err: None,
            ..dispatcher
        },
        &request,
    )
    .await;
    assert!(success.ok);
    assert!(serde_json::to_value(success)
        .unwrap()
        .get("error_detail")
        .is_none());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn disposition_new_decoder_accepts_legacy_v4_error_without_detail() {
    let response: DaemonResponseFrame = serde_json::from_str(
        r#"{
            "ok":false,"result":null,"error":"legacy failure",
            "namespace_mismatch":false,"config_mismatch":false,
            "served_config_id":"cfg","version_mismatch":false,
            "daemon_protocol_version":4,"request_id":null
        }"#,
    )
    .expect("decode legacy v4 error frame");
    assert!(response.error_detail.is_none());
    assert_eq!(response.error.as_deref(), Some("legacy failure"));
}

#[test]
fn disposition_normalization_omits_unconfirmed_domain_results() {
    for disposition in ["not_committed", "unknown", "unrecognized"] {
        let error = DaemonDispatchError::new(
            "failure",
            Some(serde_json::json!({
                "message": "failure",
                "domain_disposition": disposition,
                "domain_result": { "id": "unconfirmed" },
            })),
        );
        assert!(error.error_detail.get("domain_result").is_none());
        assert_eq!(
            error.error_detail["domain_disposition"],
            if disposition == "not_committed" {
                "not_committed"
            } else {
                "unknown"
            }
        );
    }
}

#[test]
fn disposition_normalization_iteratively_discards_deep_owned_values() {
    for disposition in ["committed", "not_committed", "unknown"] {
        let mut value = serde_json::Value::Null;
        for _ in 0..4096 {
            value = serde_json::Value::Array(vec![value]);
        }
        let fields = serde_json::Map::from_iter([
            ("domain_disposition".into(), serde_json::json!(disposition)),
            ("domain_result".into(), value),
        ]);
        let error = DaemonDispatchError::new("failure", Some(serde_json::Value::Object(fields)));
        assert!(error.error_detail.get("domain_result").is_none());
        assert_eq!(error.error_detail["domain_disposition"], disposition);
        if disposition == "committed" {
            assert_eq!(error.error_detail["code"], "result_too_deep");
        }
        serde_json::to_vec(&error.error_detail).expect("bounded error detail serializes");
    }
    let mut value = serde_json::Value::Null;
    for _ in 0..4096 {
        value = serde_json::Value::Array(vec![value]);
    }
    let error = DaemonDispatchError::new("failure", Some(value));
    assert_eq!(error.error_detail["code"], "error_detail_too_deep");
    assert_eq!(error.error_detail["domain_disposition"], "unknown");
    assert!(error.error_detail.get("data").is_none());
}

#[tokio::test]
async fn daemon_peer_disconnect_signals_request_read_cancellation() {
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatcher = CancellationAwareDispatch {
        started: Arc::clone(&started),
        cancellation_observed: Arc::clone(&cancellation_observed),
        count_sql: None,
    };
    let (mut client, server) = UnixStream::pair().expect("unix stream pair");
    let request = base_request_frame("disconnect-test");
    let payload = serde_json::to_vec(&request).expect("encode request frame");
    let handler = tokio::spawn(async move { handle_conn(server, dispatcher).await });
    write_frame(&mut client, &payload)
        .await
        .expect("write request frame");
    started.notified().await;

    drop(client);
    tokio::time::timeout(std::time::Duration::from_millis(500), handler)
        .await
        .expect("daemon handler ignored peer disconnect")
        .expect("daemon handler panicked");
    assert!(
        cancellation_observed.load(std::sync::atomic::Ordering::SeqCst),
        "peer loss did not reach the request-scoped read cancellation signal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_disconnect_interrupts_pooled_stats_count() {
    use khive_storage::{SqlAccess, SqlStatement, SqlValue};
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(khive_db::PoolConfig {
            path: Some(dir.path().join("disconnect-count.db")),
            max_readers: 1,
            ..Default::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE count_fixture(n INTEGER PRIMARY KEY); \
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) \
             INSERT INTO count_fixture SELECT x FROM n; \
             CREATE VIEW events AS SELECT 'local' AS namespace, 'knowledge.learn' AS verb \
             FROM count_fixture a CROSS JOIN count_fixture b CROSS JOIN count_fixture c;",
        )
        .unwrap();
    let sql = Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));
    let cancellation_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatcher = CancellationAwareDispatch {
        started: Arc::new(tokio::sync::Notify::new()),
        cancellation_observed: Arc::clone(&cancellation_observed),
        count_sql: Some(sql.clone()),
    };
    let (mut client, server) = UnixStream::pair().unwrap();
    let request = base_request_frame("disconnect-test");
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler = tokio::spawn(khive_db::scope_test_read_progress(
        Arc::clone(&progress),
        async move { handle_conn(server, dispatcher).await },
    ));
    write_frame(&mut client, &serde_json::to_vec(&request).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while progress.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(
                !handler.is_finished(),
                "COUNT returned before its first SQLite progress callback"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !handler.is_finished(),
        "COUNT must be outstanding at disconnect"
    );
    let started = std::time::Instant::now();
    let grace = khive_db::sqlite_interrupt_grace_from_env();
    drop(client);
    tokio::time::timeout(grace, handler)
        .await
        .expect("disconnected COUNT did not settle within interrupt grace")
        .unwrap();
    assert!(cancellation_observed.load(std::sync::atomic::Ordering::SeqCst));
    let snapshot = pool.reader_acquisition_snapshot();
    assert_eq!(snapshot.active_pooled_checkouts, 0);
    assert_eq!(snapshot.available_reader_admission_slots, 1);
    eprintln!(
        "daemon_stats_count_disconnect_ms={} grace_ms={}",
        started.elapsed().as_secs_f64() * 1000.0,
        grace.as_millis()
    );
    let count = sql
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM count_fixture".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(count, Some(SqlValue::Integer(1000))));
}

/// Protocol v4 makes `process_ref` part of dispatch semantics. A still-warm
/// v3 daemon/client pairing must fail before the verb runs; otherwise the
/// older peer can ignore the unknown field, persist a message without the
/// requested provenance, and leave the caller unable to retry safely.
#[tokio::test]
async fn protocol_v3_frame_is_rejected_before_process_ref_dispatch() {
    const {
        assert!(
            PROTOCOL_VERSION >= 4,
            "process_ref requires protocol v4 or later"
        )
    };
    let dispatch_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-v4".to_string(),
        dispatch_calls: Arc::clone(&dispatch_calls),
        pool: None,
        dispatch_err: None,
    };
    let mut request = base_request_frame("cfg-v4");
    request.protocol_version = 3;
    request.process_ref = Some("worker/legacy-rollout".to_string());

    let response = round_trip(dispatcher, &request).await;
    assert!(!response.ok);
    assert!(
        !response.version_mismatch,
        "a client below this protocol is answered in the implicit shape its bridge re-execs on"
    );
    assert_eq!(
        response.error_detail.as_ref().unwrap()["code"],
        "version_mismatch"
    );
    assert_eq!(
        response.error_detail.as_ref().unwrap()["domain_disposition"],
        "unknown"
    );
    assert_eq!(response.daemon_protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        dispatch_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a v3 frame must be rejected before a provenance-bearing mutation dispatches"
    );
    let error = response.error.expect("mismatch explains both versions");
    assert!(
        error.contains("client=3") && error.contains(&format!("daemon={PROTOCOL_VERSION}")),
        "mismatch must identify the exact rollout boundary; got {error:?}"
    );
}

/// Pre-v8 bridges compare the served config id exactly and can replay a
/// successful write locally after a daemon accepts a compatible superset.
/// Reject their requests before dispatch so a rolling upgrade cannot write twice.
#[tokio::test]
async fn protocol_v7_frame_is_rejected_before_compatible_superset_dispatch() {
    let dispatch_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let client_id = config_id("p", "");
    let daemon_id = config_id("p", "m");
    assert!(super::config_ids_compatible(&client_id, &daemon_id));
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: daemon_id,
        dispatch_calls: Arc::clone(&dispatch_calls),
        pool: None,
        dispatch_err: None,
    };
    let mut request = base_request_frame(&client_id);
    request.protocol_version = 7;

    let response = round_trip(dispatcher, &request).await;
    assert!(!response.ok);
    assert!(!response.version_mismatch);
    assert_eq!(
        response.error_detail.as_ref().unwrap()["code"],
        "version_mismatch"
    );
    assert_eq!(response.daemon_protocol_version, PROTOCOL_VERSION);
    assert_eq!(dispatch_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// A client above this protocol is answered with the explicit flag: that
/// direction is the warm-old-daemon case, where the newer client's own
/// handling replaces the daemon, and the implicit shape reserved for older
/// bridges must not reach it. A matching client is served (the round-trip
/// tests above).
#[tokio::test]
async fn newer_client_frame_is_refused_with_the_explicit_flag() {
    let dispatch_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-v4".to_string(),
        dispatch_calls: Arc::clone(&dispatch_calls),
        pool: None,
        dispatch_err: None,
    };
    let mut request = base_request_frame("cfg-v4");
    request.protocol_version = PROTOCOL_VERSION + 1;

    let response = round_trip(dispatcher, &request).await;
    assert!(!response.ok);
    assert!(
        response.version_mismatch,
        "a client above this protocol keeps the explicit flag"
    );
    assert_eq!(response.daemon_protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        response.error_detail.as_ref().unwrap()["code"],
        "version_mismatch"
    );
    assert_eq!(
        dispatch_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a newer client's frame must not dispatch"
    );
}

/// Test 1: a `metrics_only: true` request
/// returns `metrics: Some(_)` and never reaches the ops-dispatch path; a
/// normal request (the default `metrics_only: false`) still dispatches
/// exactly as before and carries no metrics. Also proves `metrics_only`
/// bypasses the `config_id` equality reject (a gauge read is
/// process-global, not namespaced to a particular client config).
#[tokio::test]
async fn metrics_only_frame_returns_snapshot_without_dispatching() {
    let dispatch_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-a".to_string(),
        dispatch_calls: Arc::clone(&dispatch_calls),
        pool: None,
        dispatch_err: None,
    };

    let mut metrics_req = base_request_frame("cfg-a");
    metrics_req.metrics_only = true;
    let metrics_resp = round_trip(dispatcher.clone(), &metrics_req).await;

    assert!(metrics_resp.ok, "metrics_only response must be ok=true");
    assert!(
        metrics_resp.metrics.is_some(),
        "metrics_only=true must return Some(snapshot)"
    );
    assert_eq!(
        dispatch_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "metrics_only must never reach the ops-dispatch path"
    );

    // metrics_only bypasses the config_id equality reject.
    let mut mismatched_req = base_request_frame("some-other-config");
    mismatched_req.metrics_only = true;
    let mismatched_resp = round_trip(dispatcher.clone(), &mismatched_req).await;
    assert!(mismatched_resp.ok);
    assert!(mismatched_resp.metrics.is_some());
    assert!(!mismatched_resp.config_mismatch);
    assert_eq!(
        dispatch_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a mismatched-config metrics_only request must still skip dispatch"
    );

    // A normal request (default metrics_only=false) is unaffected: it
    // still dispatches and carries no metrics.
    let normal_req = base_request_frame("cfg-a");
    let normal_resp = round_trip(dispatcher, &normal_req).await;
    assert!(normal_resp.ok);
    assert!(normal_resp.metrics.is_none());
    assert_eq!(dispatch_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Group/other-writable socket directories are refused whether or not the
/// sticky bit is set, and — the part that matters — are left exactly as
/// they were found.
///
/// The sticky `/tmp` shape (1777) is in the refusal set deliberately: the
/// sticky bit restricts unlinking, not creating, so a shared directory
/// lets another user pre-bind the predictable socket path before this
/// daemon starts; and a sticky directory's owner may unlink and rebind
/// regardless. Re-permissioning someone else's directory on the way past
/// is what the second assertion pins against: run as a user who *can*
/// chmod it, the old unconditional call succeeded and revoked access for
/// every other process using that directory.
#[test]
fn shared_writable_socket_dirs_are_refused_without_being_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, mode) in [("open", 0o777u32), ("sticky-tmp", 0o1777u32)] {
        let shared = dir.path().join(name);
        std::fs::create_dir(&shared).expect("create");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(mode)).expect("chmod");

        let err = ensure_socket_dir_is_trusted(&shared)
            .expect_err("group/other-writable must be refused, sticky or not");
        assert!(
            err.to_string().contains(&format!("{:04o}", mode & 0o7777)),
            "the refusal should name the mode it saw, got: {err}"
        );
        let after = std::fs::metadata(&shared)
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            after, mode,
            "refusing must not re-permission a directory khive does not own"
        );
    }
}

/// A vetted final directory is still refused when an ANCESTOR would let
/// another local user rename it away and recreate it: a non-sticky
/// group/other-writable ancestor re-roots the whole socket path without
/// the final directory's own metadata ever changing. The sticky-ancestor
/// arm (a root-owned 1777 `/tmp` above a user-owned 0700 directory is
/// acceptable) is exercised implicitly by every accepting test below,
/// whose tempdirs live under the platform temp root.
#[test]
fn writable_non_sticky_ancestor_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let open_mid = dir.path().join("open-mid");
    std::fs::create_dir(&open_mid).expect("create mid");
    let inner = open_mid.join("private");
    std::fs::create_dir(&inner).expect("create inner");
    std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    std::fs::set_permissions(&open_mid, std::fs::Permissions::from_mode(0o777)).expect("chmod mid");

    let err = ensure_socket_dir_is_trusted(&inner)
        .expect_err("a 0777 non-sticky ancestor must be refused");
    assert!(
        err.to_string().contains("ancestor"),
        "the refusal should say it was an ancestor that failed, got: {err}"
    );
    assert!(
        err.to_string().contains("open-mid"),
        "the refusal should name the failing ancestor, got: {err}"
    );
}

/// The path walk validates what a symlink component points AT, not just
/// the symlink node: a trusted (self-owned) link into a group/other-
/// writable non-sticky directory is refused on the target directory's
/// metadata, proving the walk keeps traversing — and keeps applying the
/// directory rules — past the link, exactly as the kernel will at bind
/// time.
#[test]
fn symlink_component_to_untrusted_directory_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let open = dir.path().join("open-target");
    std::fs::create_dir(&open).expect("create target");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&open, &link).expect("symlink");

    // SAFETY: `geteuid` is always successful and takes no arguments.
    let euid = unsafe { libc::geteuid() } as u32;
    let err = ensure_socket_path_is_swap_resistant(&link, euid)
        .expect_err("a link into a 0777 non-sticky directory must be refused");
    assert!(
        err.to_string().contains("open-target"),
        "the refusal should name the untrusted target directory, got: {err}"
    );
}

include!("daemon/socket_path_tests.rs");

/// Directories this daemon can trust end to end are served as found:
/// owner-only, and the umask-default 0755 every `tempfile::tempdir` and
/// test runner produces (readable/traversable but writable only by the
/// owner). On macOS these fixtures also implicitly exercise the
/// symlink-accept arm, since the platform temp root itself resolves
/// through root-owned symlinks. Foreign ownership of a directory is
/// refused by the same helper; a separate simulated-euid test covers
/// foreign ownership without requiring a privileged chown operation.
#[test]
fn trusted_socket_dirs_are_accepted_unmodified() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, mode) in [("private", 0o700), ("listable", 0o755)] {
        let d = dir.path().join(name);
        std::fs::create_dir(&d).expect("create");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).expect("chmod");

        ensure_socket_dir_is_trusted(&d)
            .unwrap_or_else(|e| panic!("mode {mode:04o} must be accepted, got: {e}"));

        let after = std::fs::metadata(&d).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(
            after, mode,
            "acceptance must not re-permission the directory either"
        );
    }
}

#[test]
fn pid_directory_owned_by_another_uid_is_refused() {
    let dir = tempfile::Builder::new()
        .prefix("khive-pid-owner-")
        .tempdir()
        .expect("tempdir");
    let parent = dir.path().join("private");
    std::fs::create_dir(&parent).expect("create private directory");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
        .expect("set private mode");

    // A non-root test cannot chown its directory to another uid. Injecting
    // an euid that does not own this directory exercises that same refusal.
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = (unsafe { libc::geteuid() } as u32).wrapping_add(1);
    let error =
        ensure_rendezvous_dir_is_trusted(&parent, RendezvousPathRole::PidFile, daemon_euid, false)
            .expect_err("a PID parent owned by another uid must be refused");
    let message = format!("{error:#}");

    assert!(
        message.contains("KHIVE_PID"),
        "wrong variable in refusal: {message}"
    );
    assert!(
        message.contains("PID-file directory") && message.contains("owned by uid"),
        "refusal must identify foreign ownership of the PID parent: {message}"
    );
}

/// Test 2: `wal_pages` reflects a real
/// checkpoint observation after writes, deterministically forced via a
/// direct `checkpoint_once` call rather than waiting on the async
/// periodic task.
#[tokio::test]
async fn metrics_snapshot_wal_pages_reflects_recent_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("metrics_wal_test.db");
    let pool = Arc::new(
        ConnectionPool::new(khive_db::PoolConfig {
            path: Some(path),
            ..khive_db::PoolConfig::for_test()
        })
        .expect("pool open"),
    );

    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE t (x INTEGER); \
                     INSERT INTO t VALUES (1); \
                     INSERT INTO t VALUES (2);",
            )
            .expect("seed writes");
    }

    let dedicated_conn = pool
        .open_standalone_writer()
        .expect("open dedicated checkpoint connection");
    khive_db::checkpoint_once(
        &pool,
        &dedicated_conn,
        &CheckpointConfig::default(),
        &mut khive_db::checkpoint::TruncateState::default(),
    )
    .expect("checkpoint_once must observe on a healthy dedicated connection");

    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-wal".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: Some(pool),
        dispatch_err: None,
    };

    let snapshot = build_metrics_snapshot(&dispatcher);
    assert!(
        snapshot.wal_pages.is_some(),
        "wal_pages must be observed after a real checkpoint tick, got {snapshot:?}"
    );
    assert_eq!(snapshot.wal_log_frames, snapshot.wal_pages);
    assert!(snapshot.wal_checkpointed_frames.is_some());
    assert!(snapshot.wal_pending_frames.is_some());
    assert!(snapshot.wal_physical_bytes.is_some());
    assert!(snapshot.wal_observed_at_unix_ms.is_some());
    assert_eq!(snapshot.wal_checkpoint_stores.len(), 1);
    assert_eq!(snapshot.wal_checkpoint_stores[0].store_id, "main");
    assert_eq!(snapshot.wal_checkpoint_stores[0].timing.ticks, 1);
    // The snapshot carries the checkpoint-pressure fields read-only
    // (no mutation path reachable through `MetricsSnapshot`/`DaemonRequestFrame`);
    // an observed tick (not a skip) must report a zero-length skip streak.
    assert_eq!(
        snapshot.wal_checkpoint_consecutive_skips, 0,
        "an observed (non-skipped) tick must report zero consecutive skips, got {snapshot:?}"
    );
}

#[derive(Clone)]
struct CheckpointMetricsDispatch {
    main: Option<Arc<ConnectionPool>>,
    secondaries: Vec<Arc<ConnectionPool>>,
}

#[async_trait]
impl DaemonDispatch for CheckpointMetricsDispatch {
    fn plan(&self, _ops: &str) -> String {
        panic!("metrics must not plan")
    }
    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<RequestIdentity>,
    ) -> Result<String, String> {
        panic!("metrics must not dispatch")
    }
    async fn warm_all(&self) {}
    fn namespace(&self) -> &str {
        "local"
    }
    fn config_id(&self) -> &str {
        "checkpoint-metrics"
    }
    fn pool_for_checkpoint(&self) -> Option<Arc<ConnectionPool>> {
        self.main.clone()
    }
    fn secondary_pools_for_checkpoint(&self) -> Vec<Arc<ConnectionPool>> {
        self.secondaries.clone()
    }
}

#[tokio::test]
#[serial(checkpoint_skip_metrics)]
async fn metrics_checkpoint_timing_keeps_stores_separate_and_scrapes_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut pools = Vec::new();
    for label in ["primary", "secondary"] {
        let directory = dir.path().join(label);
        std::fs::create_dir(&directory).unwrap();
        let pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(directory.join("same.db")),
                ..khive_db::PoolConfig::for_test()
            })
            .unwrap(),
        );
        pool.try_writer()
            .unwrap()
            .conn()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        pools.push(pool);
    }
    let dispatcher = CheckpointMetricsDispatch {
        main: Some(Arc::clone(&pools[0])),
        secondaries: vec![Arc::clone(&pools[1])],
    };
    let before = build_metrics_snapshot(&dispatcher);
    assert_eq!(before.wal_checkpoint_stores.len(), 2);
    assert!(before
        .wal_checkpoint_stores
        .iter()
        .all(|store| store.timing.ticks == 0));
    for (index, pool) in pools.iter().enumerate() {
        let conn = pool.open_standalone_writer().unwrap();
        for _ in 0..=index {
            khive_db::checkpoint_once(
                pool,
                &conn,
                &CheckpointConfig {
                    truncate_high_water_pages: u64::MAX,
                    ..CheckpointConfig::default()
                },
                &mut khive_db::checkpoint::TruncateState::default(),
            )
            .unwrap();
        }
    }
    let mut request = base_request_frame("checkpoint-metrics");
    request.metrics_only = true;
    let snapshot = round_trip(dispatcher.clone(), &request)
        .await
        .metrics
        .unwrap();
    let stores = &snapshot.wal_checkpoint_stores;
    assert_eq!(stores.len(), 2);
    assert_eq!(stores[0].store_id, "main");
    assert_eq!(stores[0].role, "main");
    assert_eq!(stores[1].store_id, "secondary:0");
    assert_eq!(stores[1].role, "secondary");
    for store in stores {
        assert_eq!(
            store.database.as_deref(),
            Some("same.db"),
            "wire label must omit directories"
        );
        assert!(store.timing.elapsed_us_max <= store.timing.elapsed_us_sum);
    }
    assert_eq!(stores[0].timing.ticks, 1);
    assert_eq!(stores[1].timing.ticks, 2);
    let again = build_metrics_snapshot(&dispatcher);
    assert_eq!(
        again.wal_checkpoint_stores, *stores,
        "scraping must not checkpoint"
    );
    let secondary_only = build_metrics_snapshot(&CheckpointMetricsDispatch {
        main: None,
        secondaries: vec![Arc::clone(&pools[1])],
    });
    assert_eq!(secondary_only.wal_checkpoint_stores.len(), 1);
    assert_eq!(
        secondary_only.wal_checkpoint_stores[0].store_id,
        "secondary:0"
    );
    assert_eq!(secondary_only.wal_checkpoint_stores[0].timing.ticks, 2);
    assert!(build_metrics_snapshot(&CheckpointMetricsDispatch {
        main: None,
        secondaries: vec![]
    })
    .wal_checkpoint_stores
    .is_empty());
}

#[test]
fn metrics_checkpoint_timing_serde_is_additive_and_round_trips() {
    let snapshot = MetricsSnapshot {
        wal_checkpoint_stores: vec![CheckpointStoreMetrics {
            store_id: "secondary:0".into(),
            role: "secondary".into(),
            database: Some("memory.db".into()),
            timing: khive_db::checkpoint::CheckpointTiming {
                ticks: 7,
                elapsed_us_sum: 123,
                elapsed_us_max: 50,
                busy_ticks: 2,
                error_ticks: 1,
            },
        }],
        ..MetricsSnapshot::default()
    };
    let wire = serde_json::to_value(&snapshot).unwrap();
    let store = &wire["wal_checkpoint_stores"][0];
    assert_eq!(
        store,
        &serde_json::json!({
            "store_id": "secondary:0", "role": "secondary", "database": "memory.db",
            "ticks": 7, "elapsed_us_sum": 123, "elapsed_us_max": 50, "busy_ticks": 2, "error_ticks": 1,
        })
    );
    assert_eq!(
        serde_json::from_value::<MetricsSnapshot>(wire.clone()).unwrap(),
        snapshot
    );
    let mut old_wire = wire.clone();
    old_wire
        .as_object_mut()
        .unwrap()
        .remove("wal_checkpoint_stores");
    let old = serde_json::from_value::<MetricsSnapshot>(old_wire).unwrap();
    assert!(
        old.wal_checkpoint_stores.is_empty(),
        "old snapshot must default the new vector"
    );
    let partial = serde_json::from_value::<CheckpointStoreMetrics>(serde_json::json!({
        "store_id": "main", "role": "main"
    }))
    .unwrap();
    assert_eq!(
        partial.timing,
        khive_db::checkpoint::CheckpointTiming::default()
    );
    assert_eq!(partial.database, None);
    #[derive(serde::Deserialize)]
    struct LegacyMetrics {
        wal_pages: Option<u64>,
        open_tx_count: usize,
    }
    let legacy: LegacyMetrics = serde_json::from_value(wire).unwrap();
    assert_eq!(legacy.wal_pages, None);
    assert_eq!(legacy.open_tx_count, 0);
}

#[tokio::test]
async fn metrics_snapshot_exposes_decomposed_writer_stages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("metrics_writer_stage_test.db");
    let pool = Arc::new(
        ConnectionPool::new(khive_db::PoolConfig {
            path: Some(path),
            ..khive_db::PoolConfig::for_test()
        })
        .expect("pool open"),
    );
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    let handle = pool
        .writer_task_handle()
        .unwrap()
        .expect("file-backed default writer task");
    handle
        .send(|conn| {
            std::thread::sleep(std::time::Duration::from_millis(30));
            conn.execute("INSERT INTO t VALUES (1)", [])
                .map_err(|error| khive_storage::error::StorageError::Pool {
                    operation: "metrics_writer_stage_test".into(),
                    message: error.to_string(),
                })
        })
        .await
        .unwrap();

    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-writer-stages".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: Some(pool),
        dispatch_err: None,
    };
    let snapshot = build_metrics_snapshot(&dispatcher);
    assert!(snapshot.write_last_queue_wait_micros.is_some());
    assert!(snapshot.write_last_transaction_acquire_micros.is_some());
    assert!(snapshot.write_last_commit_micros.is_some());
    assert!(
        snapshot.write_last_body_micros >= Some(25_000),
        "synthetic delay must be attributed to the body: {snapshot:?}"
    );
    assert!(snapshot.write_last_total_micros >= snapshot.write_last_body_micros);
    assert!(snapshot.write_last_observed_at_unix_ms.is_some());
}

/// Test 3: the tx-pin oracle. The registry is process-global, so an
/// unrelated transaction can depart between snapshots and exactly offset
/// this test's registration. Keep an owned handle live and assert the
/// resulting count floor instead of comparing two points in time.
#[test]
#[serial(tx_registry)]
fn metrics_snapshot_reflects_open_transaction_registry() {
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-tx".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };

    let departing_handle = khive_storage::tx_registry::register(Some(
        "daemon_metrics_snapshot_departing_test_tx".to_string(),
    ));
    let before = build_metrics_snapshot(&dispatcher).open_tx_count;
    assert!(before >= 1);

    let handle = khive_storage::tx_registry::register(Some(
        "daemon_metrics_snapshot_owned_test_tx".to_string(),
    ));
    drop(departing_handle);

    let during = build_metrics_snapshot(&dispatcher);
    assert!(
        during.open_tx_count >= 1,
        "open_tx_count must reflect the live owned transaction despite registry churn: \
             churn_baseline={before} during={}",
        during.open_tx_count
    );
    assert!(
        during.oldest_pinned_tx_micros.is_some(),
        "oldest_pinned_tx_micros must be Some while a transaction is open"
    );

    drop(handle);
    assert!(
        !khive_storage::tx_registry::snapshot()
            .iter()
            .any(|(_, label)| label.as_deref() == Some("daemon_metrics_snapshot_owned_test_tx")),
        "the owned registry entry must disappear when its handle is dropped"
    );
}

/// Test 4: write-queue depth is flag-gated
/// on `PoolConfig::write_queue_enabled` (the `KHIVE_WRITE_QUEUE=1`
/// setting), never on a specific depth value (racy under concurrency).
#[tokio::test]
async fn metrics_snapshot_write_queue_depth_flag_gated() {
    let dir = tempfile::tempdir().expect("tempdir");

    let enabled_pool = Arc::new(
        ConnectionPool::new(khive_db::PoolConfig {
            path: Some(dir.path().join("wq_enabled.db")),
            write_queue_enabled: Some(true),
            ..khive_db::PoolConfig::for_test()
        })
        .expect("pool open"),
    );
    let enabled_dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-wq-on".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: Some(enabled_pool),
        dispatch_err: None,
    };
    let snapshot_on = build_metrics_snapshot(&enabled_dispatcher);
    assert!(
        snapshot_on.write_queue_depth.is_some(),
        "write_queue_depth must be Some when write_queue_enabled=true, got {snapshot_on:?}"
    );
    assert!(snapshot_on.write_queue_capacity.is_some());

    let disabled_pool = Arc::new(
        ConnectionPool::new(khive_db::PoolConfig {
            path: Some(dir.path().join("wq_disabled.db")),
            write_queue_enabled: Some(false),
            ..khive_db::PoolConfig::for_test()
        })
        .expect("pool open"),
    );
    let disabled_dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-wq-off".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: Some(disabled_pool),
        dispatch_err: None,
    };
    let snapshot_off = build_metrics_snapshot(&disabled_dispatcher);
    assert!(
        snapshot_off.write_queue_depth.is_none(),
        "write_queue_depth must be None when write_queue_enabled=false, got {snapshot_off:?}"
    );
    assert!(snapshot_off.write_queue_capacity.is_none());

    // No pool at all (in-memory/poolless dispatcher): also None.
    let no_pool_dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-no-pool".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let snapshot_no_pool = build_metrics_snapshot(&no_pool_dispatcher);
    assert!(snapshot_no_pool.write_queue_depth.is_none());
    assert!(snapshot_no_pool.write_queue_capacity.is_none());
}

/// Test 5: serde default back-compat in both directions — a request JSON
/// without additive request fields deserializes to their defaults, and a
/// response JSON without `metrics` (an old daemon's shape) deserializes
/// with it `None`.
#[test]
fn frame_serde_defaults_additive_fields_when_absent() {
    let req_json = serde_json::json!({
        "ops": "",
        "presentation": null,
        "presentation_per_op": null,
        "namespace": "local",
        "actor_id": null,
        "visible_namespaces": [],
        "config_id": "cfg",
        "protocol_version": PROTOCOL_VERSION,
        "probe_only": false,
        "format": null,
        "format_per_op": null,
        "from_wire": false
    });
    let frame: DaemonRequestFrame =
        serde_json::from_value(req_json).expect("decode a metrics_only-absent request frame");
    assert!(
        !frame.metrics_only,
        "metrics_only must default to false when absent from the wire payload"
    );
    assert_eq!(
        frame.request_id, None,
        "request_id must default to None when absent from the wire payload (khive#948)"
    );
    assert_eq!(
        frame.process_ref, None,
        "process_ref must default to None when absent from the wire payload (khive#1428)"
    );
    let encoded_frame = serde_json::to_value(&frame).expect("encode request frame");
    assert!(
        encoded_frame.get("process_ref").is_none(),
        "absent provenance must not change the serialized request wire shape"
    );

    let resp_json = serde_json::json!({
        "ok": true,
        "result": null,
        "error": null,
        "namespace_mismatch": false,
        "config_mismatch": false,
        "served_config_id": "cfg",
        "version_mismatch": false,
        "daemon_protocol_version": PROTOCOL_VERSION
    });
    let resp: DaemonResponseFrame =
        serde_json::from_value(resp_json).expect("decode a metrics-absent response frame");
    assert!(
        resp.metrics.is_none(),
        "metrics must default to None when absent from the wire payload"
    );
    assert_eq!(
        resp.request_id, None,
        "request_id must default to None when absent from the wire payload (khive#948)"
    );
}

/// khive#948: a request carrying `request_id: Some(n)` gets back a
/// response with `request_id: Some(n)` on both the success and the
/// error/denied dispatch arms — the echo must survive every branch of
/// `handle_conn`, not only the happy path.
#[tokio::test]
async fn request_id_echoed_on_success_and_error_arms() {
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-a".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let mut ok_req = base_request_frame("cfg-a");
    ok_req.request_id = Some(42);
    let ok_resp = round_trip(dispatcher, &ok_req).await;
    assert!(ok_resp.ok, "expected successful dispatch: {ok_resp:?}");
    assert_eq!(
        ok_resp.request_id,
        Some(42),
        "request_id must be echoed back on a successful dispatch response"
    );

    // config_mismatch is a rejection arm that never reaches dispatch —
    // must still echo the id so the client can join the failure.
    let mismatched_dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-a".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let mut mismatch_req = base_request_frame("cfg-WRONG");
    mismatch_req.request_id = Some(99);
    let mismatch_resp = round_trip(mismatched_dispatcher, &mismatch_req).await;
    assert!(mismatch_resp.config_mismatch);
    assert_eq!(
        mismatch_resp.request_id,
        Some(99),
        "request_id must be echoed on the config_mismatch rejection arm too"
    );

    // The real ops-dispatch error arm (`Err(e)` from `dispatcher.dispatch`)
    // must echo the id as well, not only the pre-dispatch rejection arms.
    let erroring_dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "cfg-a".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: Some("simulated dispatch error".to_string()),
    };
    let mut err_req = base_request_frame("cfg-a");
    err_req.request_id = Some(7);
    let err_resp = round_trip(erroring_dispatcher, &err_req).await;
    assert!(!err_resp.ok, "expected a dispatch error: {err_resp:?}");
    assert_eq!(
        err_resp.request_id,
        Some(7),
        "request_id must be echoed on the real ops-dispatch error arm"
    );
}

// ── owner-checked shutdown cleanup ────────────────────────────────────────
//
// A draining daemon must not unlink a socket/PID pair that a replacement
// daemon has already bound. These tests exercise `shutdown_cleanup_if_owned`
// directly (the pure decision the caller makes under the recovery lock)
// rather than driving `run_daemon`'s real SIGTERM shutdown, which would
// require sending a signal to the whole test process.

#[test]
fn shutdown_cleanup_removes_paths_it_still_owns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let identity = socket_identity(&sock);
    assert!(
        identity.is_some(),
        "must read identity of a freshly bound socket"
    );

    let cleaned = shutdown_cleanup_if_owned(&sock, &pid_file, identity);

    assert!(
        cleaned,
        "cleanup must proceed when PID and socket still match"
    );
    assert!(!sock.exists(), "owned socket must be removed");
    assert!(!pid_file.exists(), "owned pid file must be removed");
}

#[test]
fn shutdown_cleanup_skips_when_pid_file_names_a_different_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind socket");
    let identity = socket_identity(&sock);
    // A concurrent client's kill_and_respawn already replaced the PID file
    // with a different (replacement daemon's) PID before this daemon's
    // drain completed.
    std::fs::write(&pid_file, "1").expect("write foreign pid file");

    let cleaned = shutdown_cleanup_if_owned(&sock, &pid_file, identity);

    assert!(
        !cleaned,
        "cleanup must be skipped when the PID file no longer names this process"
    );
    assert!(sock.exists(), "replacement daemon's socket must survive");
    assert!(
        pid_file.exists(),
        "replacement daemon's pid file must survive"
    );
}

#[test]
fn shutdown_cleanup_skips_when_socket_was_rebound_by_a_replacement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let original_sock = dir.path().join("original.sock");
    let pid_file = dir.path().join("khived.pid");

    // Bind two sockets at DIFFERENT paths, both alive at the same time,
    // so the OS cannot recycle an inode between them the way it could
    // across a bind/drop/rebind cycle at a single path (the flakiness a
    // prior version of this test hit on some filesystems). Both
    // identities are captured through the real production
    // `socket_identity()` path, not a synthetic/sentinel value, so a
    // regression where `socket_identity()` returns a constant identity
    // for every socket makes the `assert!` below fail loudly instead of
    // silently passing.
    let _original_listener =
        std::os::unix::net::UnixListener::bind(&original_sock).expect("bind original socket");
    let _replacement_listener =
        std::os::unix::net::UnixListener::bind(&sock).expect("bind replacement socket");

    let original_identity = socket_identity(&original_sock);
    let replacement_identity = socket_identity(&sock);
    assert!(
        original_identity.is_some(),
        "must read identity of the original socket"
    );
    assert!(
        replacement_identity.is_some(),
        "must read identity of the replacement socket"
    );
    assert!(
        original_identity != replacement_identity,
        "two concurrently bound sockets must have distinct identities"
    );

    std::fs::write(&pid_file, std::process::id().to_string())
        .expect("write pid file matching this process");

    // `sock` (the replacement bind's path) is checked against
    // `original_identity` (a different, concurrently-alive socket's
    // identity) - the mismatch alone must be enough to block cleanup,
    // even though the pid file matches this process.
    let cleaned = shutdown_cleanup_if_owned(&sock, &pid_file, original_identity);

    assert!(
        !cleaned,
        "cleanup must be skipped when the socket at this path is a different \
             inode than the one this daemon originally bound"
    );
    assert!(sock.exists(), "replacement daemon's socket must survive");
    assert!(
        pid_file.exists(),
        "replacement daemon's pid file must survive"
    );
}

#[test]
fn shutdown_cleanup_preserves_atomically_renamed_successor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let staged_sock = dir.path().join("next.sock");
    let pid_file = dir.path().join("khived.pid");
    let _original_listener =
        std::os::unix::net::UnixListener::bind(&sock).expect("bind original socket");
    let successor =
        std::os::unix::net::UnixListener::bind(&staged_sock).expect("bind staged successor");
    let original_identity = socket_identity(&sock).expect("original socket identity");
    let successor_identity = socket_identity(&staged_sock).expect("successor socket identity");
    assert!(original_identity != successor_identity);
    let original_pid = std::process::id().to_string();
    std::fs::write(&pid_file, &original_pid).expect("write original PID");

    std::fs::rename(&staged_sock, &sock).expect("publish successor over original socket");
    assert!(!staged_sock.exists());
    assert!(socket_identity(&sock) == Some(successor_identity));
    // A matching PID must not authorize deleting a different socket inode.
    assert!(!shutdown_cleanup_if_owned(
        &sock,
        &pid_file,
        Some(original_identity)
    ));
    assert!(socket_identity(&sock) == Some(successor_identity));
    assert_eq!(
        std::fs::read_to_string(&pid_file).expect("PID must survive stale cleanup"),
        original_pid
    );
    successor
        .set_nonblocking(true)
        .expect("bound successor must support nonblocking accept");
    let _client = std::os::unix::net::UnixStream::connect(&sock)
        .expect("published successor must remain reachable");
    let _accepted = successor
        .accept()
        .expect("successor must receive connection");
}

#[test]
fn isolated_daemon_locks_use_private_fixture_paths() {
    if crate::test_process::run_in_child() {
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("child HOME"));
    for path in [lock_path(), recoverer_lock_path()] {
        assert_eq!(
            path.parent(),
            home.parent(),
            "runtime daemon locks must use private fixture paths outside HOME"
        );
    }
    let _boot = acquire_daemon_boot_guard().expect("private boot lock");
    let _recoverer = try_acquire_recoverer_lock_until(
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .expect("private recoverer lock")
    .expect("private recoverer lock must be available");
    assert!(lock_path().is_file());
    assert!(recoverer_lock_path().is_file());
    assert!(
        std::fs::read_dir(home).unwrap().next().is_none(),
        "both daemon lock producers must leave the child HOME empty"
    );
}

include!("daemon/store_guard_tests.rs");

// ── the recovery lock actually serializes two boot sequences ─────────────
//
// Production wiring (`khive_mcp::serve::run` / `serve_server`) now acquires
// this same lock *before* building a `KhiveMcpServer` (which runs
// migrations and applies pack schema plans / FTS DDL) and holds it through
// daemon bind+pid-write, via `run_daemon_with_boot_guard`. That closes the
// cold-boot race only if `acquire_recovery_lock` genuinely provides mutual
// exclusion across concurrent boot attempts — this test proves the
// primitive itself: two "boot sequences" (each holding the lock across a
// simulated schema-init critical section) must never run their critical
// sections at the same time.
#[test]
#[serial]
fn recovery_lock_serializes_two_concurrent_boot_sequences() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let overlap_detected = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let run_one_boot = |active: Arc<std::sync::atomic::AtomicUsize>,
                        overlap: Arc<std::sync::atomic::AtomicBool>| {
        move || {
            let _guard = acquire_recovery_lock().expect("acquire recovery lock");
            // Enter the "schema-init" critical section.
            if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 0 {
                overlap.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            // `_guard` drops here, releasing the lock.
        }
    };

    let t1 = std::thread::spawn(run_one_boot(active.clone(), overlap_detected.clone()));
    let t2 = std::thread::spawn(run_one_boot(active.clone(), overlap_detected.clone()));
    t1.join().expect("boot thread 1 must not panic");
    t2.join().expect("boot thread 2 must not panic");

    assert!(
        !overlap_detected.load(std::sync::atomic::Ordering::SeqCst),
        "two concurrent boot sequences must never hold the schema-init \
             critical section at the same time (#667)"
    );

    std::env::remove_var("KHIVE_LOCK");
}

// ── acquire_daemon_boot_guard treats lock failure as fatal ───────────────
// (unlike best-effort acquire_recovery_lock, whose `None` on failure is
// correct for its own best-effort callers).

#[test]
#[serial]
fn acquire_daemon_boot_guard_returns_guard_when_lock_available() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    let guard = acquire_daemon_boot_guard();
    assert!(
        guard.is_ok(),
        "daemon boot guard must succeed when the lock file can be opened and flocked"
    );
    drop(guard);

    std::env::remove_var("KHIVE_LOCK");
}

#[test]
#[serial]
fn acquire_daemon_boot_guard_fails_loudly_when_lock_file_cannot_be_opened() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    // Point KHIVE_LOCK at a directory, not a file: opening a directory
    // with `write(true)` fails (EISDIR), so `acquire_recovery_lock`
    // returns `None` here — the exact failure mode
    // `acquire_daemon_boot_guard` must turn into a hard `Err` instead of
    // silently letting daemon-mode boot proceed unguarded.
    std::env::set_var("KHIVE_LOCK", dir.path());

    let result = acquire_daemon_boot_guard();
    assert!(
        result.is_err(),
        "daemon boot guard must fail loudly, never silently proceed unguarded, \
             when the underlying recovery lock cannot be acquired"
    );

    std::env::remove_var("KHIVE_LOCK");
}

// ── write_pid_file_exclusive never truncates a winner's pid file ────────

#[test]
fn write_pid_file_exclusive_creates_new_file_with_own_pid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    write_pid_file_exclusive(&pid_file).expect("first writer must win");
    let contents = std::fs::read_to_string(&pid_file).expect("read pid file");
    assert_eq!(contents, std::process::id().to_string());
}

#[test]
fn write_pid_file_exclusive_refuses_to_overwrite_an_existing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    std::fs::write(&pid_file, "999999").expect("seed an existing pid file");

    let err = write_pid_file_exclusive(&pid_file)
        .expect_err("must not silently overwrite an existing pid file");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

    // The existing content must be completely untouched — proving this is
    // `create_new`, not the old `create(true).truncate(true)`.
    let contents = std::fs::read_to_string(&pid_file).expect("read pid file");
    assert_eq!(
        contents, "999999",
        "an existing pid file must never be truncated by a losing writer"
    );
}

// Real (not simulated) concurrency: two OS threads race to `create_new`
// the exact same path, synchronized with a `Barrier` so they genuinely
// overlap at the syscall rather than relying on a sleep-based ordering
// guess. This is the deterministic race oracle for the convergence
// requirement the atomic-creation primitive `write_pid_file_exclusive`
// is built on: exactly one of two simultaneous daemon starters may claim
// the pid file, and the loser must see `AlreadyExists`, never silently
// clobber the winner's content.
#[test]
fn two_concurrent_writers_converge_on_exactly_one_pid_file_owner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = std::sync::Arc::new(dir.path().join("khived.pid"));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let spawn_writer = |pid_file: std::sync::Arc<std::path::PathBuf>,
                        barrier: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            write_pid_file_exclusive(&pid_file)
        })
    };

    let t1 = spawn_writer(pid_file.clone(), barrier.clone());
    let t2 = spawn_writer(pid_file.clone(), barrier.clone());
    let r1 = t1.join().expect("writer 1 must not panic");
    let r2 = t2.join().expect("writer 2 must not panic");

    let results = [&r1, &r2];
    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let already_exists_count = results
        .iter()
        .filter(|r| matches!(r, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists))
        .count();
    assert_eq!(
        ok_count, 1,
        "exactly one of two concurrent writers must win the pid file"
    );
    assert_eq!(
        already_exists_count, 1,
        "the other writer must observe AlreadyExists, never a silent overwrite"
    );
    assert!(pid_file.exists(), "the winner's pid file must exist");
    let contents = std::fs::read_to_string(&*pid_file).expect("read pid file");
    assert_eq!(
        contents,
        std::process::id().to_string(),
        "the surviving pid file must contain the winner's pid — both threads \
             share this process's pid, so an unexpected value would also prove a \
             lost/garbled write raced through"
    );
}

// ── connection principal (ADR-096 condition 2) ────────────────────────────

/// The peer-credential syscall must actually work on this platform and
/// report the real uid, not error or return a placeholder.
///
/// This matters more than it looks because the accept path fails CLOSED: if
/// `peer_uid` errored unconditionally — wrong syscall, wrong socket option,
/// an unimplemented platform arm — every connection would be refused and
/// the daemon would be silently unreachable. A test that only exercised the
/// decision function would not catch that.
#[tokio::test]
async fn peer_uid_reports_the_connecting_process_uid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&sock).expect("bind");

    let connect_path = sock.clone();
    let client = tokio::spawn(async move { UnixStream::connect(&connect_path).await });

    let (server_side, _) = listener.accept().await.expect("accept");
    let client_side = client.await.expect("join").expect("connect");

    // SAFETY: `geteuid` is always successful and takes no arguments.
    let expected = unsafe { libc::geteuid() } as u32;

    assert_eq!(
        peer_uid(&server_side).expect("peer_uid must succeed on a live connection"),
        expected,
        "the uid read from the kernel for a same-process connection must be \
             this process's euid"
    );
    // Symmetric: both ends report the same peer on a same-uid connection.
    assert_eq!(
        peer_uid(&client_side).expect("peer_uid must succeed on the client end"),
        expected
    );
}

/// The refusal rule itself: the principal is the uid, and only a foreign
/// uid is refused.
///
/// The second assertion is the load-bearing one and it is a regression
/// guard, not a formality. ADR-096 shipped many `actor_id`s over one
/// socket; every seat on a normal host is a distinct attribution at the
/// same uid. A check that conflated attribution with principal would refuse
/// them all, so "same uid is permitted" must stay true no matter how the
/// rule is later tightened.
#[test]
fn only_a_foreign_uid_is_refused() {
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let euid = unsafe { libc::geteuid() } as u32;

    assert!(
        uid_is_permitted(euid, euid),
        "a connection from the daemon's own uid must be served — this is \
             every seat on the host, and ADR-096 accepted exactly this shape"
    );
    assert!(
        !uid_is_permitted(euid.wrapping_add(1), euid),
        "a connection from any other uid must be refused"
    );
    assert!(
        !uid_is_permitted(0, euid.wrapping_add(1)),
        "root is not special-cased: the rule is equality with the daemon's \
             euid, not a privilege comparison"
    );
}

/// Captures one tracing event's fields as `name=value ` text, so a test can
/// assert on what an operator reading the log actually sees rather than on
/// the value the log line was formatted from.
struct CapturedFields(Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::Subscriber for CapturedFields {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={:?} ", field.name(), value));
            }
        }
        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

// Shares the process-wide background-task statics with the counter tests
// above; see the `#[serial(background_tasks)]` note there.
#[cfg(unix)]
#[tokio::test]
#[serial(background_tasks)]
async fn drain_timeout_warning_names_the_outstanding_tasks() {
    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CapturedFields(lines.clone());
    let _dispatch = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));

    let active = std::sync::atomic::AtomicUsize::new(0);
    let (stop_tx, stop_rx) = tokio::sync::broadcast::channel::<()>(1);
    for name in ["test_task_alpha", "test_task_beta"] {
        let mut rx = stop_rx.resubscribe();
        track_named_background_task(name, async move {
            let _ = rx.recv().await;
        });
    }
    drop(stop_rx);

    let drained = drain_with_timeout(&active, std::time::Duration::from_millis(150)).await;
    assert!(
        !drained,
        "two unfinished tasks must make the drain time out"
    );

    let warned = lines
        .lock()
        .unwrap()
        .iter()
        .find(|line| line.contains("drain timeout reached"))
        .cloned()
        .expect("the drain timeout must emit its warning through the test subscriber");
    assert!(
        warned.contains("test_task_alpha") && warned.contains("test_task_beta"),
        "the drain-timeout warning must name every outstanding task; got {warned}"
    );

    let _ = stop_tx.send(());
    for _ in 0..100 {
        if background_task_names().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// Shares the process-wide background-task statics; see the
// `#[serial(background_tasks)]` note above.
#[tokio::test]
#[serial(background_tasks)]
async fn a_named_task_drops_its_name_when_it_finishes() {
    let before = background_task_count();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    track_named_background_task("test_task_finishes", async move {
        let _ = rx.await;
    });
    assert!(
        background_task_names().contains(&"test_task_finishes".to_string()),
        "a live named task must be listed while the counter holds it"
    );
    tx.send(()).expect("still awaiting");
    for _ in 0..100 {
        if background_task_count() == before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(background_task_count(), before);
    assert!(
        !background_task_names().contains(&"test_task_finishes".to_string()),
        "a finished task's name must be released, not left to accumulate"
    );
}

// Shares the process-wide background-task statics; see the
// `#[serial(background_tasks)]` note above.
#[tokio::test]
#[serial(background_tasks)]
async fn the_unnamed_entry_point_still_registers_and_releases() {
    let before = background_task_count();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    track_background_task(async move {
        let _ = rx.await;
    });
    assert_eq!(background_task_count(), before + 1);
    assert!(
        background_task_names().contains(&UNNAMED_BACKGROUND_TASK.to_string()),
        "the unchanged public entry point must still register, under the placeholder name"
    );
    tx.send(()).expect("still awaiting");
    for _ in 0..100 {
        if background_task_count() == before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(background_task_count(), before);
}

include!("daemon_config_id_tests.rs");
