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
            .autocommit(|conn| {
                Ok(conn.execute_batch(
                    "CREATE TABLE t (x INTEGER); \
                     INSERT INTO t VALUES (1); \
                     INSERT INTO t VALUES (2);",
                )?)
            })
            .expect("seed writes");
    }

    let dedicated_conn = pool
        .open_standalone_checkpoint()
        .expect("open dedicated checkpoint connection");
    dedicated_conn
        .run(
            &CheckpointConfig::default(),
            &mut khive_db::checkpoint::TruncateState::default(),
        )
        .expect("checkpoint_once must observe on a healthy dedicated connection");
    drop(dedicated_conn);

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
            .autocommit(|conn| {
                Ok(conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")?)
            })
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
        let conn = pool.open_standalone_checkpoint().unwrap();
        for _ in 0..=index {
            conn.run(
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
            .autocommit(|conn| Ok(conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")?))
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
