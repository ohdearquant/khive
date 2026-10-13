use super::*;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use std::sync::atomic::{AtomicU32, Ordering};
use tempfile::TempDir;

fn tmp_db() -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("khive-test.db");
    let path = path.to_str().expect("utf8 path").to_string();
    (dir, path)
}

#[cfg(unix)]
use khive_storage::test_support::freeze_snapshot_sidecars;

async fn make_server(db_path: &str) -> KhiveMcpServer {
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(db_path)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        actor_id: Some("actor:component-test".to_string()),
        packs: vec!["kg".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime");
    KhiveMcpServer::new(rt).expect("server")
}

async fn wait_for_state(
    health: &HealthReporter,
    name: &str,
    state: ComponentState,
) -> ComponentStatus {
    for _ in 0..400 {
        if let Some(s) = health.status(name) {
            if s.state == state {
                return s;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "component {name} never reached {state:?}; last = {:?}",
        health.status(name)
    );
}

#[derive(Debug, Default)]
struct UploadSweepProbe {
    calls: AtomicU32,
}

#[async_trait::async_trait]
impl khive_storage::BlobStore for UploadSweepProbe {
    async fn put(
        &self,
        _bytes: Vec<u8>,
    ) -> khive_storage::StorageResult<khive_storage::ContentRef> {
        panic!("sweeper must not publish objects")
    }

    async fn get_bounded_verified(
        &self,
        _content_ref: &khive_storage::ContentRef,
        _max_bytes: u64,
    ) -> khive_storage::StorageResult<Vec<u8>> {
        panic!("sweeper must not read committed objects")
    }

    async fn exists(
        &self,
        _content_ref: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<bool> {
        panic!("sweeper must not inspect committed objects")
    }

    async fn size(
        &self,
        _content_ref: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<Option<u64>> {
        panic!("sweeper must not inspect committed objects")
    }

    async fn delete(
        &self,
        _content_ref: &khive_storage::ContentRef,
    ) -> khive_storage::StorageResult<bool> {
        panic!("sweeper must not delete committed objects")
    }

    async fn sweep_uploads(&self, _idle_for: Duration) -> khive_storage::StorageResult<u64> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call <= 6 {
            Err(khive_storage::StorageError::Internal(
                "transient upload sweep failure".to_string(),
            ))
        } else {
            Ok(0)
        }
    }
}

fn blob_server(runtime: KhiveRuntime) -> KhiveMcpServer {
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_blob::BlobPack::new(runtime));
    KhiveMcpServer::from_registry(builder.build().expect("blob registry"))
}

#[test]
#[serial_test::serial(config_ledger)]
fn blob_upload_roster_uses_the_registered_manager_and_requires_an_available_store() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let unavailable = blob_server(runtime);
    assert!(unavailable.blob_upload_manager().is_none());

    let empty_registry = khive_runtime::VerbRegistryBuilder::new()
        .build()
        .expect("empty registry");
    let absent = KhiveMcpServer::from_registry(empty_registry);
    assert!(absent.blob_upload_manager().is_none());

    let runtime = KhiveRuntime::memory().expect("runtime");
    runtime
        .install_blob_store(Arc::new(UploadSweepProbe::default()))
        .expect("install blob store");
    let server = blob_server(runtime);
    let manager = server.blob_upload_manager().expect("upload manager");
    assert!(Arc::ptr_eq(
        &manager,
        &server
            .clone()
            .blob_upload_manager()
            .expect("cloned manager")
    ));
    let roster = component_registrations(None, Some(manager));
    assert_eq!(
        roster
            .iter()
            .filter(|reg| reg.name == BLOB_UPLOAD_COMPONENT_NAME)
            .count(),
        1
    );
    assert!(
        component_registrations(None, unavailable.blob_upload_manager())
            .iter()
            .all(|reg| reg.name != BLOB_UPLOAD_COMPONENT_NAME)
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn blob_upload_roster_obeys_its_own_runtime_mode_in_mixed_deployments() {
    let (_dir, db) = tmp_db();
    let config = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(db)),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["blob".to_string()],
        ..Default::default()
    };
    KhiveRuntime::new(config.clone()).expect("initialize database");
    #[cfg(unix)]
    freeze_snapshot_sidecars(config.db_path.as_ref().expect("db path"));
    let read_only = KhiveRuntime::new_readonly(config).expect("read-only runtime");
    read_only
        .install_blob_store(Arc::new(UploadSweepProbe::default()))
        .expect("install read-only blob store");
    let before = read_only.backend().pool().writer_acquisition_snapshot();
    let writable = KhiveRuntime::memory().expect("writable runtime");
    writable
        .install_blob_store(Arc::new(UploadSweepProbe::default()))
        .expect("install writable blob store");

    let read_only_blob = blob_server(read_only.clone()).with_runtime(writable.clone());
    assert!(read_only_blob.blob_upload_manager().is_none());
    let writable_blob = blob_server(writable).with_runtime(read_only.clone());
    assert!(writable_blob.blob_upload_manager().is_some());
    assert_eq!(
        read_only.backend().pool().writer_acquisition_snapshot(),
        before,
        "upload admission must not probe a read-only backend through a writer"
    );
}

#[tokio::test(start_paused = true)]
async fn blob_upload_sweeper_delays_retries_skips_missed_ticks_and_joins_on_cancel() {
    let store = Arc::new(UploadSweepProbe::default());
    let runtime = KhiveRuntime::memory().expect("runtime");
    runtime
        .install_blob_store(store.clone())
        .expect("install blob store");
    let server = blob_server(runtime);
    let manager = server.blob_upload_manager().expect("upload manager");
    let interval = manager.sweep_interval();
    let health = HealthReporter::default();
    let parent = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(
        blob_upload_component_registration(manager),
        server,
        parent.child_token(),
        health.clone(),
    ));
    tokio::task::yield_now().await;
    assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(interval - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);

    for expected in 2..=7 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
        assert_eq!(store.calls.load(Ordering::SeqCst), expected);
    }
    let status = health.status(BLOB_UPLOAD_COMPONENT_NAME).expect("status");
    assert_eq!(status.state, ComponentState::Running);
    assert_eq!(
        status.restart_count, 0,
        "backend errors must not spend restart budget"
    );
    assert!(status.last_heartbeat.is_some());

    tokio::time::advance(interval * 4).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(
        store.calls.load(Ordering::SeqCst),
        8,
        "missed ticks must not burst"
    );
    parent.cancel();
    supervisor.await.expect("supervisor joins");
    let status = health.status(BLOB_UPLOAD_COMPONENT_NAME).expect("status");
    assert_eq!(status.state, ComponentState::Stopped);
    tokio::time::advance(interval * 3).await;
    assert_eq!(
        store.calls.load(Ordering::SeqCst),
        8,
        "no sweep may outlive shutdown"
    );
}

#[test]
fn shutdown_wait_is_clamped_strictly_inside_the_drain_window() {
    let drain_ms = 10_000;
    // A request equal to the drain bound is the failure case: the grace
    // wait would consume the whole window with no time left for the
    // abort and terminal state record.
    assert_eq!(
        clamped_shutdown_wait_ms(drain_ms, drain_ms),
        drain_ms - SHUTDOWN_DRAIN_MARGIN_MS
    );
    assert_eq!(
        clamped_shutdown_wait_ms(u64::MAX, drain_ms),
        drain_ms - SHUTDOWN_DRAIN_MARGIN_MS
    );
    // Requests already inside the bound pass through unchanged.
    assert_eq!(clamped_shutdown_wait_ms(100, drain_ms), 100);
    assert_eq!(
        clamped_shutdown_wait_ms(drain_ms - SHUTDOWN_DRAIN_MARGIN_MS, drain_ms),
        drain_ms - SHUTDOWN_DRAIN_MARGIN_MS
    );
    // A drain window smaller than the margin degrades to an immediate
    // abort rather than underflowing.
    assert_eq!(
        clamped_shutdown_wait_ms(100, SHUTDOWN_DRAIN_MARGIN_MS / 2),
        0
    );
}

#[test]
fn restart_delay_including_jitter_never_exceeds_hard_cap() {
    for base in [1, 1_000, 30_000, 59_999, 60_000, u64::MAX] {
        assert!(
            restart_delay_ms(base, SCHEDULE_BACKOFF_MAX_MS) <= SCHEDULE_BACKOFF_MAX_MS,
            "base={base} exceeded the documented 60-second schedule restart cap"
        );
    }
    assert_eq!(
        restart_delay_ms(u64::MAX, u64::MAX),
        u64::MAX,
        "saturating jitter arithmetic must remain overflow-safe"
    );
}

#[test]
fn tmp_db_guard_removes_database_and_all_sidecars() {
    let (dir, db_path) = tmp_db();
    let dir_path = dir.path().to_path_buf();
    for suffix in ["", "-wal", "-shm", ".khive-blob-gc.lock"] {
        std::fs::write(format!("{db_path}{suffix}"), b"fixture").expect("write fixture");
    }

    drop(dir);

    assert!(
        !dir_path.exists(),
        "dropping the directory guard must remove the database and every sibling sidecar"
    );
}

#[tokio::test]
async fn empty_registration_set_is_a_no_op() {
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    let started = start_components(&[], &server, CancellationToken::new(), health.clone());
    assert_eq!(started, 0);
    assert!(health.snapshot().is_empty());
}

#[test]
#[serial_test::serial(config_ledger)]
fn schedule_roster_contains_exactly_one_dynamic_component_when_resolved() {
    let (_f, db) = tmp_db();
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(db)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime");

    let absent = component_registrations(None, None)
        .into_iter()
        .filter(|reg| reg.name == SCHEDULE_COMPONENT_NAME)
        .count();
    let present: Vec<_> = component_registrations(Some(rt), None)
        .into_iter()
        .filter(|reg| reg.name == SCHEDULE_COMPONENT_NAME)
        .collect();

    assert_eq!(absent, 0, "pack-absent roster must omit schedule-tick");
    assert_eq!(
        present.len(),
        1,
        "resolved schedule pack contributes one ticker"
    );
    let reg = &present[0];
    assert_eq!(reg.restart, RestartClass::OnFailure);
    assert_eq!(reg.max_restarts, 5);
    assert_eq!(reg.backoff_initial_ms, 1_000);
    assert_eq!(reg.backoff_max_ms, 60_000);
    assert_eq!(reg.shutdown_timeout_ms, 5_000);
}

#[test]
#[serial_test::serial(config_ledger)]
fn schedule_roster_omits_read_only_assigned_runtime_without_writer_acquisition() {
    let (_f, db) = tmp_db();
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(db)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    };
    KhiveRuntime::new(cfg.clone()).expect("create and migrate snapshot source");
    #[cfg(unix)]
    freeze_snapshot_sidecars(cfg.db_path.as_ref().expect("db path"));
    let read_only = KhiveRuntime::new_readonly(cfg).expect("open schedule snapshot read-only");
    let before = read_only.backend().pool().writer_acquisition_snapshot();

    let schedule_count = component_registrations(Some(read_only.clone()), None)
        .into_iter()
        .filter(|reg| reg.name == SCHEDULE_COMPONENT_NAME)
        .count();

    assert_eq!(
        schedule_count, 0,
        "a read-only schedule backend must not launch the writer-dependent ticker"
    );
    assert_eq!(
        read_only.backend().pool().writer_acquisition_snapshot(),
        before,
        "component roster construction must not probe a read-only backend through a writer"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn supervised_schedule_component_heartbeats_and_stops_cooperatively() {
    let (_f, db) = tmp_db();
    let cfg = RuntimeConfig {
        db_path: Some(std::path::PathBuf::from(&db)),
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..Default::default()
    };
    let rt = KhiveRuntime::new(cfg).expect("runtime");
    let server = KhiveMcpServer::new(rt.clone()).expect("server");
    let health = HealthReporter::default();
    let parent = CancellationToken::new();

    let started = start_component_registrations(
        vec![schedule_component_registration(
            rt,
            Duration::from_millis(10),
        )],
        &server,
        parent.clone(),
        health.clone(),
    );
    assert_eq!(started, 1);
    for _ in 0..400 {
        if health
            .status(SCHEDULE_COMPONENT_NAME)
            .is_some_and(|status| status.last_heartbeat.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        health
            .status(SCHEDULE_COMPONENT_NAME)
            .is_some_and(|status| status.last_heartbeat.is_some()),
        "quiet agenda drains must still prove liveness"
    );

    parent.cancel();
    let status = wait_for_state(&health, SCHEDULE_COMPONENT_NAME, ComponentState::Stopped).await;
    assert_eq!(status.restart_count, 0);
    assert!(status.last_error.is_none());
    let stopped_heartbeat = status
        .last_heartbeat
        .expect("schedule component heartbeated before shutdown");
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        health
            .status(SCHEDULE_COMPONENT_NAME)
            .and_then(|status| status.last_heartbeat),
        Some(stopped_heartbeat),
        "the inner ticker must be joined before the supervisor reports Stopped; no \
             schedule task may survive component shutdown"
    );
}

static DUP_A_RUNS: AtomicU32 = AtomicU32::new(0);
static DUP_B_RUNS: AtomicU32 = AtomicU32::new(0);
fn dup_a(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        DUP_A_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}
fn dup_b(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        DUP_B_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

#[tokio::test]
async fn duplicate_names_are_flagged_but_both_components_still_start() {
    static REG_A: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-dup",
        restart: RestartClass::Never,
        max_restarts: 0,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: dup_a,
    };
    static REG_B: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-dup",
        restart: RestartClass::Never,
        max_restarts: 0,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: dup_b,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    let started = start_components(
        &[&REG_A, &REG_B],
        &server,
        CancellationToken::new(),
        health.clone(),
    );
    assert_eq!(started, 2);
    // The collision is reported (error log), never resolved by dropping a
    // registration: both components must run.
    wait_for_state(&health, "test-dup", ComponentState::Stopped).await;
    for _ in 0..400 {
        if DUP_A_RUNS.load(Ordering::SeqCst) == 1 && DUP_B_RUNS.load(Ordering::SeqCst) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "both duplicate-named components should have started; a={} b={}",
        DUP_A_RUNS.load(Ordering::SeqCst),
        DUP_B_RUNS.load(Ordering::SeqCst)
    );
}

static CLEAN_RUNS: AtomicU32 = AtomicU32::new(0);
fn clean_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        CLEAN_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

#[tokio::test]
async fn clean_completion_is_terminal_stopped_without_restart() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-clean",
        restart: RestartClass::OnFailure,
        max_restarts: 5,
        backoff_initial_ms: 1,
        backoff_max_ms: 4,
        shutdown_timeout_ms: 100,
        start: clean_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    start_components(&[&REG], &server, CancellationToken::new(), health.clone());
    let status = wait_for_state(&health, "test-clean", ComponentState::Stopped).await;
    assert_eq!(status.restart_count, 0);
    assert_eq!(CLEAN_RUNS.load(Ordering::SeqCst), 1);
}

static RETRY_RUNS: AtomicU32 = AtomicU32::new(0);
fn retryable_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        RETRY_RUNS.fetch_add(1, Ordering::SeqCst);
        Err(ComponentError::Retryable("boom".into()))
    })
}

#[tokio::test]
async fn retryable_failures_consume_budget_then_terminal_unhealthy() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-retry",
        restart: RestartClass::OnFailure,
        max_restarts: 2,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: retryable_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    start_components(&[&REG], &server, CancellationToken::new(), health.clone());
    let status = wait_for_state(&health, "test-retry", ComponentState::Unhealthy).await;
    // initial run + 2 budgeted restarts, then terminal — no hot loop.
    assert_eq!(RETRY_RUNS.load(Ordering::SeqCst), 3);
    assert_eq!(status.restart_count, 2);
    assert_eq!(status.last_error.as_deref(), Some("boom"));
}

static PERMANENT_RUNS: AtomicU32 = AtomicU32::new(0);
fn permanent_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        PERMANENT_RUNS.fetch_add(1, Ordering::SeqCst);
        Err(ComponentError::Permanent("bad config".into()))
    })
}

#[tokio::test]
async fn permanent_failure_is_immediately_terminal_despite_budget() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-permanent",
        restart: RestartClass::OnFailure,
        max_restarts: 5,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: permanent_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    start_components(&[&REG], &server, CancellationToken::new(), health.clone());
    let status = wait_for_state(&health, "test-permanent", ComponentState::Unhealthy).await;
    assert_eq!(PERMANENT_RUNS.load(Ordering::SeqCst), 1);
    assert_eq!(status.restart_count, 0);
}

fn never_restart_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async { Err(ComponentError::Retryable("one shot".into())) })
}

#[tokio::test]
async fn restart_class_never_makes_any_failure_terminal() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-never",
        restart: RestartClass::Never,
        max_restarts: 5,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: never_restart_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    start_components(&[&REG], &server, CancellationToken::new(), health.clone());
    let status = wait_for_state(&health, "test-never", ComponentState::Unhealthy).await;
    assert_eq!(status.restart_count, 0);
}

fn cooperative_component(ctx: HostContext) -> ComponentFuture {
    Box::pin(async move {
        loop {
            tokio::select! {
                _ = ctx.cancellation().cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(2)) => ctx.heartbeat(),
            }
        }
    })
}

#[tokio::test]
async fn cooperative_cancellation_stops_cleanly_and_heartbeats_recorded() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-coop",
        restart: RestartClass::OnFailure,
        max_restarts: 5,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 500,
        start: cooperative_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    let parent = CancellationToken::new();
    start_components(&[&REG], &server, parent.clone(), health.clone());
    // Let it run a few cycles so a heartbeat lands.
    for _ in 0..200 {
        if health
            .status("test-coop")
            .is_some_and(|s| s.last_heartbeat.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    parent.cancel();
    let status = wait_for_state(&health, "test-coop", ComponentState::Stopped).await;
    assert_eq!(status.restart_count, 0);
    assert!(status.last_heartbeat.is_some());
    assert!(status.last_error.is_none());
}

fn hung_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        // Ignores cancellation entirely.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(())
    })
}

#[tokio::test]
async fn hung_component_is_aborted_after_shutdown_timeout() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-hung",
        restart: RestartClass::OnFailure,
        max_restarts: 5,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 30,
        start: hung_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    let parent = CancellationToken::new();
    start_components(&[&REG], &server, parent.clone(), health.clone());
    // Give it a moment to start, then order shutdown.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let before = std::time::Instant::now();
    parent.cancel();
    // An abort after ignoring cancellation is a real wedge — terminally
    // unhealthy with the abort recorded, never a clean stop.
    let status = wait_for_state(&health, "test-hung", ComponentState::Unhealthy).await;
    assert!(status.last_error.as_deref().unwrap().contains("aborted"));
    // Bounded: well under the hour the component wanted.
    assert!(before.elapsed() < Duration::from_secs(5));
}

static OVERFLOW_RUNS: AtomicU32 = AtomicU32::new(0);
fn overflow_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        OVERFLOW_RUNS.fetch_add(1, Ordering::SeqCst);
        Err(ComponentError::Retryable(
            "push the backoff arithmetic".into(),
        ))
    })
}

#[tokio::test]
async fn extreme_backoff_values_never_panic_the_supervisor() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-overflow",
        restart: RestartClass::OnFailure,
        max_restarts: 3,
        backoff_initial_ms: u64::MAX,
        backoff_max_ms: u64::MAX,
        shutdown_timeout_ms: u64::MAX,
        start: overflow_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    let parent = CancellationToken::new();
    start_components(&[&REG], &server, parent.clone(), health.clone());
    // First failure lands Degraded, then the supervisor sits in a
    // (saturating, non-panicking) enormous backoff sleep.
    let status = wait_for_state(&health, "test-overflow", ComponentState::Degraded).await;
    assert_eq!(status.restart_count, 0);
    assert_eq!(OVERFLOW_RUNS.load(Ordering::SeqCst), 1);
    // Cancellation during backoff still stops cleanly — proving the
    // supervisor survived the arithmetic instead of panicking past
    // Degraded.
    parent.cancel();
    wait_for_state(&health, "test-overflow", ComponentState::Stopped).await;
}

static PANIC_RUNS: AtomicU32 = AtomicU32::new(0);
fn panicking_component(_ctx: HostContext) -> ComponentFuture {
    Box::pin(async {
        PANIC_RUNS.fetch_add(1, Ordering::SeqCst);
        panic!("component panic");
    })
}

#[tokio::test]
async fn panic_is_isolated_and_classified_retryable() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-panic",
        restart: RestartClass::OnFailure,
        max_restarts: 1,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: panicking_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    start_components(&[&REG], &server, CancellationToken::new(), health.clone());
    let status = wait_for_state(&health, "test-panic", ComponentState::Unhealthy).await;
    assert_eq!(PANIC_RUNS.load(Ordering::SeqCst), 2);
    assert!(status.last_error.as_deref().unwrap().contains("panic"));
}

#[tokio::test]
async fn synchronous_factory_panic_is_terminal_and_sibling_survives() {
    let healthy_cycles = Arc::new(AtomicU32::new(0));
    let healthy_cycles_for_start = healthy_cycles.clone();
    let healthy = ComponentRegistration {
        name: "test-isolated-healthy",
        restart: RestartClass::Never,
        max_restarts: 0,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: Arc::new(move |ctx: HostContext| -> ComponentFuture {
            let cycles = healthy_cycles_for_start.clone();
            Box::pin(async move {
                loop {
                    tokio::select! {
                        _ = ctx.cancellation().cancelled() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_millis(2)) => {
                            cycles.fetch_add(1, Ordering::SeqCst);
                            ctx.heartbeat();
                        }
                    }
                }
            })
        }),
    };
    let failing = ComponentRegistration {
        name: "test-isolated-failing",
        restart: RestartClass::Never,
        max_restarts: 0,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: Arc::new(|_ctx: HostContext| -> ComponentFuture {
            panic!("synchronous component construction panic");
        }),
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    let parent = CancellationToken::new();
    assert_eq!(
        start_component_registrations(
            vec![failing, healthy],
            &server,
            parent.clone(),
            health.clone(),
        ),
        2
    );

    let failed = wait_for_state(&health, "test-isolated-failing", ComponentState::Unhealthy).await;
    assert_eq!(
        failed.restart_count, 0,
        "construction failures are terminal and must not spend restart budget"
    );
    assert!(
        failed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("construction panicked")),
        "construction panic must be named in terminal health: {failed:?}"
    );
    for _ in 0..400 {
        if healthy_cycles.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        healthy_cycles.load(Ordering::SeqCst) >= 2,
        "one component's construction panic must not cancel or starve an independent sibling"
    );
    assert_eq!(
        health
            .status("test-isolated-healthy")
            .map(|status| status.state),
        Some(ComponentState::Running)
    );

    parent.cancel();
    wait_for_state(&health, "test-isolated-healthy", ComponentState::Stopped).await;
}

static DISPATCH_OK: AtomicU32 = AtomicU32::new(0);
fn dispatching_component(ctx: HostContext) -> ComponentFuture {
    Box::pin(async move {
        // The in-process dispatch path — the surface that skips the
        // wire-only Subhandler visibility gate (daemon-internal
        // capability; the ingest class in distribution builds).
        let params = crate::tools::request::RequestParams {
            ops: "create(kind=\"concept\", name=\"component-dispatch-probe\") \
                      | get(id=$prev.id)"
                .to_string(),
            ..Default::default()
        };
        let out = ctx
            .server()
            .dispatch_request_local(params)
            .await
            .map_err(|e| ComponentError::Retryable(e.to_string()))?;
        if out.contains("component-dispatch-probe") {
            DISPATCH_OK.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    })
}

#[tokio::test]
async fn host_context_dispatch_handle_lands_a_write() {
    static REG: DaemonComponentRegistration = DaemonComponentRegistration {
        name: "test-dispatch",
        restart: RestartClass::Never,
        max_restarts: 0,
        backoff_initial_ms: 1,
        backoff_max_ms: 2,
        shutdown_timeout_ms: 100,
        start: dispatching_component,
    };
    let (_f, db) = tmp_db();
    let server = make_server(&db).await;
    let health = HealthReporter::default();
    start_components(&[&REG], &server, CancellationToken::new(), health.clone());
    let status = wait_for_state(&health, "test-dispatch", ComponentState::Stopped).await;
    assert!(status.last_error.is_none(), "dispatch failed: {status:?}");
    assert_eq!(DISPATCH_OK.load(Ordering::SeqCst), 1);
}
