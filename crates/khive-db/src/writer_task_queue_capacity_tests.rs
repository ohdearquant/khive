#[tokio::test]
async fn full_channel_applies_backpressure_not_immediate_error() {
    // Build the channel directly (bypassing `spawn`/`run_writer_task`)
    // so nothing ever drains it — deterministic control over "the
    // channel is full" instead of racing a real writer task's
    // processing speed.
    let (tx, _rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
    let handle = WriterTaskHandle {
        tx,
        backend_key: None,
        db: "test".to_string(),
        slow_write_threshold: None,
        enqueue_timeout: Duration::from_secs(5),
    };

    // First send fills the sole channel slot. Its reply never arrives
    // since nothing drains `_rx`, so run it in the background.
    let first = tokio::spawn({
        let handle = handle.clone();
        async move {
            let _ = handle.send(|_conn| Ok::<(), StorageError>(())).await;
        }
    });

    // Give the first send a moment to occupy the channel slot.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Second send must block (backpressure), not fail immediately: a
    // short timeout should elapse rather than resolve.
    let second = tokio::time::timeout(
        Duration::from_millis(100),
        handle.send(|_conn| Ok::<(), StorageError>(())),
    )
    .await;

    assert!(
        second.is_err(),
        "a full channel must apply backpressure (send suspends) rather \
         than erroring immediately — no try_send escape hatch per ADR-067"
    );

    first.abort();
}

#[tokio::test]
async fn send_with_timeout_maps_full_channel_to_write_queue_full() {
    let (tx, _rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
    let handle = WriterTaskHandle {
        tx,
        backend_key: None,
        db: "test".to_string(),
        slow_write_threshold: None,
        enqueue_timeout: Duration::from_secs(5),
    };

    let first = tokio::spawn({
        let handle = handle.clone();
        async move {
            let _ = handle.send(|_conn| Ok::<(), StorageError>(())).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let result = handle
        .send_with_timeout(
            |_conn| Ok::<(), StorageError>(()),
            Duration::from_millis(50),
        )
        .await;

    match result {
        Err(StorageError::WriteQueueFull { timeout_ms }) => assert_eq!(timeout_ms, 50),
        other => panic!("expected WriteQueueFull, got {other:?}"),
    }

    first.abort();
}

#[tokio::test]
async fn configured_enqueue_timeout_rejects_only_unaccepted_request() {
    // A real file-backed writer task: `send_bounded` reuses
    // `PoolConfig::write_admission_deadline_ms` (ADR-131 Decision 2) as
    // its enqueue deadline, captured at `spawn`, so this must exercise
    // the actual spawn path rather than a hand-built channel (#1382).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("configured_enqueue_timeout.db");
    let cfg = PoolConfig {
        path: Some(path.clone()),
        write_admission_deadline_ms: 100,
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    let handle = spawn(&pool, 1).expect("writer task should spawn on a file-backed pool");

    // Request A: dequeued and running (inside `spawn_blocking`), blocked
    // on a test-controlled channel so the writer task's single drain
    // slot stays occupied deterministically — no sleeps.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let handle_a = handle.clone();
    let a_task = tokio::spawn(async move {
        handle_a
            .send(move |_conn| {
                let _ = started_tx.send(());
                release_rx.recv().expect("test must release request A");
                Ok::<(), StorageError>(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("request A did not start")
        .expect("request A dropped its start signal");

    // Request B: A has been dequeued (freeing the one channel slot), so
    // the private `enqueue` helper proves B is accepted and now occupies
    // that slot, without waiting for A to finish.
    let b_reply_rx = tokio::time::timeout(
        Duration::from_secs(5),
        handle.enqueue(|_conn| Ok::<(), StorageError>(())),
    )
    .await
    .expect("B must be accepted promptly")
    .expect("B must be accepted: the one channel slot is free while A drains");

    // Request C: the channel is now full (A draining, B queued behind
    // it) — `send_bounded` must reject C on the configured
    // `write_admission_deadline_ms` without ever running its closure.
    let c_ran = Arc::new(AtomicBool::new(false));
    let c_ran_in_op = Arc::clone(&c_ran);
    let c_result = handle
        .send_bounded(move |_conn| {
            c_ran_in_op.store(true, Ordering::SeqCst);
            Ok::<(), StorageError>(())
        })
        .await;
    match c_result {
        Err(StorageError::WriteQueueFull { .. }) => {}
        other => panic!("expected WriteQueueFull, got {other:?}"),
    }
    assert!(!c_ran.load(Ordering::SeqCst), "C must never run");

    // Release A; both A and B must then complete normally.
    release_tx.send(()).expect("release request A");
    tokio::time::timeout(Duration::from_secs(5), a_task)
        .await
        .expect("A did not complete")
        .expect("A task join")
        .expect("A must complete successfully");
    tokio::time::timeout(Duration::from_secs(5), b_reply_rx)
        .await
        .expect("B did not reply")
        .expect("B's reply channel must not be dropped")
        .expect("B must complete successfully");
}

// `#[serial(tx_registry)]`: this test deliberately keeps a request (and
// thus its `writer_task_tx` registry handle) alive past a timeout, so it is
// the worst polluter of the checkpoint `tx_age_sweep_*` reads if left
// un-serialized. Shares the key — see the note on
// `begin_immediate_failure_replies_error_without_running_op`.
#[tokio::test]
#[serial(tx_registry)]
async fn send_with_timeout_returns_op_result_when_op_outlives_the_timeout() {
    // `send_with_timeout`'s timeout must bound ONLY the enqueue step —
    // never the reply-wait. An accepted request (channel not full) must
    // run to completion and report its REAL result even when that takes
    // longer than `timeout`; before this fix, wrapping the whole
    // send-plus-reply-wait in one timeout would misreport this as
    // `WriteQueueFull` despite the write actually landing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_slow_op.db");
    // A lock directory of its own keeps this deliberate writer hold off the
    // volume lease the other tests in this process share.
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }

    let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

    let result = handle
        .send_with_timeout(
            |conn| {
                // Deliberately slower than the timeout below: proves the
                // reply-wait itself is never bounded by `timeout`.
                std::thread::sleep(Duration::from_millis(150));
                conn.execute("INSERT INTO t (id, v) VALUES (1, 'slow')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_insert".into(),
                        message: e.to_string(),
                    })
            },
            Duration::from_millis(20),
        )
        .await;

    let affected = result.expect(
        "an accepted request must return its real result even when the \
         op takes longer than the enqueue timeout, not WriteQueueFull",
    );
    assert_eq!(affected, 1);

    // The slow op's write must have actually committed, not just been
    // reported as successful.
    let reader = pool.reader().expect("reader");
    let v: String = reader
        .conn()
        .query_row("SELECT v FROM t WHERE id = 1", [], |row| row.get(0))
        .expect("the slow op's write must have committed");
    assert_eq!(v, "slow");
}
