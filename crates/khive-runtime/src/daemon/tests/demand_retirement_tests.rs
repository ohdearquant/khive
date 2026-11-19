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

fn pool_has_open_transaction(pool: &ConnectionPool) -> bool {
    let khive_storage::tx_registry::TxOrigin::Database(identity) = pool.origin() else {
        panic!("the transaction-registry fixture must be file-backed");
    };
    khive_storage::tx_registry::oldest_for(&khive_storage::tx_registry::TxOriginFilter::Secondary(
        identity,
    ))
    .is_some()
}

#[test]
#[serial(background_tasks, tx_registry)]
fn pool_transaction_probe_detects_own_and_ignores_foreign_database() {
    let dir = tempfile::tempdir().unwrap();
    let own = ConnectionPool::new(khive_db::PoolConfig {
        path: Some(dir.path().join("own.db")),
        write_queue_enabled: Some(false),
        write_routing_strict: false,
        ..Default::default()
    })
    .unwrap();
    let foreign = ConnectionPool::new(khive_db::PoolConfig {
        path: Some(dir.path().join("foreign.db")),
        write_queue_enabled: Some(false),
        write_routing_strict: false,
        ..Default::default()
    })
    .unwrap();
    assert_ne!(own.origin(), foreign.origin());

    let own_writer = own.writer().unwrap();
    own_writer
        .transaction(|_| {
            assert!(
                pool_has_open_transaction(&own),
                "the probe must detect a live transaction on its own database"
            );
            assert!(!pool_has_open_transaction(&foreign));
            Ok(())
        })
        .unwrap();
    assert!(!pool_has_open_transaction(&own));

    let foreign_writer = foreign.writer().unwrap();
    foreign_writer
        .transaction(|_| {
            assert!(pool_has_open_transaction(&foreign));
            assert!(
                !pool_has_open_transaction(&own),
                "the probe must ignore a live transaction on a foreign database"
            );
            Ok(())
        })
        .unwrap();
    assert!(!pool_has_open_transaction(&foreign));
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks, tx_registry, active_phases)]
async fn retained_raw_sql_writer_blocks_actual_idle_wait_and_persistent_stays() {
    // Unrelated tests must not contribute process-wide transaction spans.
    if crate::test_process::run_in_child() {
        return;
    }
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
        !pool_has_open_transaction(&pool),
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
    if tokio::time::timeout(std::time::Duration::from_secs(2), idle)
        .await
        .is_err()
    {
        panic!(
            "released writer must let the idle wait finish; snapshot {:?}, blockers now {:?}, open spans {:?}",
            demand.snapshot(),
            idle_retirement_blockers(&d),
            khive_storage::tx_registry::snapshot()
        );
    }
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
    assert!(
        pooled.try_idle(|| idle_retirement_blockers(&d)),
        "released pooled writer must allow retirement; snapshot {:?}, blockers now {:?}, open spans {:?}",
        pooled.snapshot(),
        idle_retirement_blockers(&d),
        khive_storage::tx_registry::snapshot()
    );
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks, tx_registry, active_phases)]
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
#[serial(background_tasks, tx_registry, active_phases)]
async fn explicit_sql_reader_transaction_blocks_retirement_without_writer_hold() {
    // Unrelated tests must not contribute process-wide transaction spans.
    if crate::test_process::run_in_child() {
        return;
    }
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
    assert!(
        state.try_idle(|| idle_retirement_blockers(&d)),
        "ended reader transaction must allow retirement; snapshot {:?}, blockers now {:?}, open spans {:?}",
        state.snapshot(),
        idle_retirement_blockers(&d),
        khive_storage::tx_registry::snapshot()
    );
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks, tx_registry, active_phases)]
async fn unsettled_named_worker_blocks_idle_without_resetting_clock() {
    // Unrelated tests must not contribute process-wide transaction spans.
    if crate::test_process::run_in_child() {
        return;
    }
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
        "maintenance completion must not reset ordinary activity; snapshot {:?}, blockers now {:?}, open spans {:?}",
        state.snapshot(),
        idle_retirement_blockers(&d),
        khive_storage::tx_registry::snapshot()
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
