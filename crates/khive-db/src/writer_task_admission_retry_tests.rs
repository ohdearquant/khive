// `#[serial(tx_registry)]`: same rationale as
// `begin_immediate_failure_replies_error_without_running_op`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(tx_registry)]
async fn transient_begin_refusal_retries_once_and_restores_timeout() {
    // Force the first contended BEGIN to return immediately, leaving
    // budget for the Rust retry. The holder is released only after that
    // refusal is counted, so an uncontended first attempt cannot pass.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_begin_transient_contention.db");
    let busy_timeout = Duration::from_secs(5);
    let configured_timeout_ms = i64::try_from(busy_timeout.as_millis()).unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        busy_timeout,
        ..PoolConfig::for_test()
    })
    .unwrap();
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    let handle = spawn(&pool, 8).expect("writer task spawn");
    let begin_attempts = Arc::new(AtomicUsize::new(0));
    let begin_attempts_in_setup = Arc::clone(&begin_attempts);
    handle
        .send_top_level(move |conn| {
            // A successful timeout reduction installs SQLite's normal busy
            // handler for attempt two; only the first refusal is immediate.
            conn.busy_handler(None)
                .map_err(|error| StorageError::Internal(error.to_string()))?;
            count_begin_attempts(conn, begin_attempts_in_setup)
                .map_err(|error| StorageError::Internal(error.to_string()))
        })
        .await
        .expect("install connection-local contention observers");
    let lock_holder = pool.try_writer().unwrap();
    lock_holder.conn().execute_batch("BEGIN IMMEDIATE").unwrap();

    let op_runs = Arc::new(AtomicUsize::new(0));
    let op_runs_in_request = Arc::clone(&op_runs);
    let send_future = handle.send(move |conn| {
        op_runs_in_request.fetch_add(1, Ordering::SeqCst);
        conn.execute("INSERT INTO t (id) VALUES (1)", [])
            .map_err(|error| StorageError::Pool {
                operation: "test_insert_after_transient_contention".into(),
                message: error.to_string(),
            })?;
        conn.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
            .map_err(|error| StorageError::Internal(error.to_string()))
    });
    let release_future = async {
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            while pool.writer_acquisition_snapshot().writer_task_begin_busy == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        // Release even when the handshake times out, so a failing test
        // cannot strand the writer behind its own fixture lock.
        lock_holder.conn().execute_batch("ROLLBACK").unwrap();
        observed.expect("first BEGIN refusal must be observed before releasing the lock");
    };
    let (result, ()) = tokio::join!(send_future, release_future);

    assert_eq!(
        result.expect("BEGIN IMMEDIATE succeeds once the transient lock clears"),
        configured_timeout_ms,
        "the configured timeout must be restored before the operation runs"
    );
    assert_eq!(
        begin_attempts.load(Ordering::SeqCst),
        2,
        "the request must actually retry BEGIN"
    );
    assert_eq!(
        op_runs.load(Ordering::SeqCst),
        1,
        "the FnOnce request closure must execute exactly once"
    );

    let settled = pool.writer_acquisition_snapshot();
    assert_eq!(
        settled.writer_task_begin_busy, 1,
        "the first real BEGIN refusal must be observed"
    );
    assert_eq!(settled.writer_task_begin_busy_absorbed, 1);
    let next_timeout = handle
        .send_top_level(|conn| {
            conn.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
                .map_err(|error| StorageError::Internal(error.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(
        next_timeout, configured_timeout_ms,
        "the next request must retain the configured timeout"
    );
    assert_eq!(
        begin_attempts.load(Ordering::SeqCst),
        2,
        "timeout probes and top-level setup must not count as BEGIN attempts"
    );
    let reader = pool.reader().unwrap();
    let rows: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "exactly one closure execution commits one row");
}

fn count_begin_attempts(conn: &Connection, attempts: Arc<AtomicUsize>) -> rusqlite::Result<()> {
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(
            context.action,
            AuthAction::Transaction {
                operation: TransactionOperation::Begin
            }
        ) {
            attempts.fetch_add(1, Ordering::SeqCst);
        }
        Authorization::Allow
    }))
}

#[test]
fn failed_busy_timeout_reduction_stops_before_a_second_begin() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("writer_task_timeout_update_failure.db"));
    let conn = pool.open_standalone_writer_untracked().unwrap();
    let busy_timeout = Duration::from_secs(5);
    // Return BUSY before the budget expires, without manufacturing the
    // BEGIN result. Only the timeout setter below injects a failure.
    conn.busy_handler(None).unwrap();
    let original_timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    count_begin_attempts(&conn, Arc::clone(&attempts)).unwrap();
    let lock_holder = pool.try_writer().unwrap();
    lock_holder.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
    let counters = pool.writer_acquisition_counters();
    let mut timeout_updates = Vec::new();

    let (result, _, reported_attempts) =
        begin_immediate_with_retry(&conn, &counters, busy_timeout, |_, timeout| {
            timeout_updates.push(timeout);
            Err(rusqlite::Error::InvalidQuery)
        });

    let error = result.expect_err("failed timeout reduction must surface the busy refusal");
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy),
        "preserve the original BEGIN error, not the injected setter error"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "failed reduction must not issue a second BEGIN"
    );
    assert_eq!(reported_attempts as usize, attempts.load(Ordering::SeqCst));
    assert_eq!(timeout_updates.len(), 1, "a failed first update must not trigger retries or a spurious restoration call through the injected setter");
    assert!(timeout_updates[0] < busy_timeout);
    assert!(!timeout_updates[0].is_zero());
    let unchanged_timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        unchanged_timeout, original_timeout,
        "the failed setter did not change the connection timeout"
    );
    assert!(
        conn.is_autocommit(),
        "the refused acquisition must not open a transaction"
    );
    let snapshot = pool.writer_acquisition_snapshot();
    assert_eq!(snapshot.writer_task_begin_busy, 1);
    assert_eq!(
        snapshot.writer_task_begin_busy_absorbed, 0,
        "an unretried refusal must not be counted as absorbed"
    );

    lock_holder.conn().execute_batch("ROLLBACK").unwrap();
    drop(lock_holder);
    conn.busy_timeout(busy_timeout).unwrap();
    let (positive, _, reported_attempts) =
        begin_immediate_with_retry(&conn, &counters, busy_timeout, Connection::busy_timeout);
    positive
        .expect("the same connection and observer must see a valid BEGIN once contention clears");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        reported_attempts, 1,
        "attempt count is local to this request"
    );
    conn.execute_batch("ROLLBACK").unwrap();
}

// `#[serial(tx_registry)]`: same rationale as
// `begin_immediate_failure_replies_error_without_running_op`.
#[tokio::test]
#[serial(tx_registry)]
async fn contended_begin_exhaustion_separates_absorbed_and_surfaced_refusals() {
    // Real contention: a refused write must be COUNTED, not merely
    // reported to its caller. Before this counter, `db_diagnostics`
    // showed a clean writer while requests were being refused after a
    // full busy timeout, so an operator could not distinguish this
    // daemon from one that had never refused a write.
    //
    // The lock is held continuously for the whole request, so SQLite's
    // own busy handler already spends the entire configured
    // `busy_timeout` internally before this call returns BUSY — the
    // shared retry budget introduced to bound total acquisition time to
    // one window (see `begin_retry_budget_makes_exactly_one_attempt_under_sustained_contention`)
    // is therefore already spent the instant this first refusal
    // surfaces, and no further attempt is retried. Real, sustained
    // contention against a single busy_timeout window can only ever
    // produce exactly one busy refusal and zero absorbed ones; a
    // multi-refusal absorbed sequence requires a refusal shape that
    // returns before the timeout elapses (for example SQLITE_LOCKED),
    // which this integration test cannot reproduce deterministically.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_begin_busy_counter.db");
    let cfg = PoolConfig {
        path: Some(path.clone()),
        busy_timeout: Duration::from_millis(150),
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

    let before = pool.writer_acquisition_snapshot();
    assert_eq!(
        before.writer_task_begin_busy, 0,
        "baseline: nothing has been refused yet"
    );
    assert_eq!(before.writer_task_begin_busy_absorbed, 0);

    let lock_holder = pool.try_writer().unwrap();
    lock_holder.conn().execute_batch("BEGIN IMMEDIATE").unwrap();

    let result = handle
        .send(|conn| {
            conn.execute("INSERT INTO t (id) VALUES (1)", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert".into(),
                    message: e.to_string(),
                })
        })
        .await;
    assert!(
        matches!(&result, Err(StorageError::WriterTaskBusy { .. })),
        "precondition: the request must actually be refused busy, got {result:?}"
    );

    let after = pool.writer_acquisition_snapshot();
    assert_eq!(
        after.writer_task_begin_busy, 1,
        "the single refusal the caller was told about must still be counted"
    );
    assert_eq!(
        after.writer_task_begin_busy_absorbed, 0,
        "a refusal that already spent the whole shared budget waiting out \
             SQLite's own busy handler must not be retried, so nothing is absorbed"
    );
    assert_eq!(
        after.writer_task_begin_errors, 0,
        "a busy refusal must not be counted as a non-busy BEGIN error"
    );
    assert_eq!(
        after.timeouts, before.timeouts,
        "a writer-task BEGIN refusal must not be mislabeled as a pool-mutex \
             checkout timeout — separate ADR-135 F6 stages, separate counters"
    );

    // Discriminating arm: a SUCCEEDING request must not move the failure
    // counter. Without this the assertion above would also pass against a
    // counter that simply counted every request.
    lock_holder.conn().execute_batch("ROLLBACK").unwrap();
    drop(lock_holder);
    handle
        .send(|conn| {
            conn.execute("INSERT INTO t (id) VALUES (2)", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert_after_busy".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("the writer task survives transient contention");

    let settled = pool.writer_acquisition_snapshot();
    assert_eq!(
        settled.writer_task_begin_busy, 1,
        "a successful request must leave the refusal counter untouched"
    );
    assert_eq!(
        settled.writer_task_begin_busy_absorbed, 0,
        "an uncontended request must not move the absorbed counter"
    );
    assert!(
        settled.writer_task_acquisitions > after.writer_task_acquisitions,
        "and it must still register as a success"
    );
}

// `#[serial(tx_registry)]`: same rationale as
// `begin_immediate_failure_replies_error_without_running_op`.
#[tokio::test]
#[serial(tx_registry)]
async fn begin_retry_budget_makes_exactly_one_attempt_under_sustained_contention() {
    // Before this fix, three BEGIN attempts each ran under their own
    // full `busy_timeout`, so persistent contention could hold the
    // serialized writer for roughly three windows plus the 15 ms of
    // explicit backoff. A held writer lock that is never released for
    // the life of this test reproduces that persistent contention:
    // total time from send to the final refusal must stay within one
    // busy_timeout window plus the sleeps, not three.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_begin_retry_budget.db");
    let busy_timeout = Duration::from_millis(150);
    let cfg = PoolConfig {
        path: Some(path),
        busy_timeout,
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");
    let lock_holder = pool.try_writer().unwrap();
    lock_holder.conn().execute_batch("BEGIN IMMEDIATE").unwrap();

    let result = handle
        .send(|conn| {
            conn.execute("INSERT INTO t (id) VALUES (1)", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert".into(),
                    message: e.to_string(),
                })
        })
        .await;

    assert!(
        matches!(&result, Err(StorageError::WriterTaskBusy { .. })),
        "precondition: the request must actually be refused busy, got {result:?}"
    );
    // Against a lock that is never released, SQLite's busy handler spends
    // the whole configured window inside the first `BEGIN IMMEDIATE`, so
    // the shared budget is exhausted the moment that refusal surfaces:
    // exactly one attempt is made and nothing is absorbed. Three
    // unbounded attempts would have recorded two absorbed refusals and
    // waited roughly three windows. The attempt count is the bound's
    // deterministic signature; wall-clock time is not asserted because
    // scheduler delay on a loaded host dwarfs a 150 ms window.
    let counters = pool.writer_acquisition_snapshot();
    assert_eq!(
        counters.writer_task_begin_busy, 1,
        "one busy refusal must surface after the shared budget is spent"
    );
    assert_eq!(
        counters.writer_task_begin_busy_absorbed, 0,
        "a refusal that already consumed the whole budget must not be retried"
    );

    lock_holder.conn().execute_batch("ROLLBACK").unwrap();
}

#[tokio::test]
#[serial(tx_registry)]
async fn writer_task_resamples_capacity_for_each_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_capacity.db");
    let mut pool = file_pool(&path);
    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        match observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Ok(102),
            1 => Ok(100),
            extra => panic!("unexpected capacity sample {extra}"),
        }
    });
    let operations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = spawn(&pool, 8).unwrap();

    let first_count = Arc::clone(&operations);
    handle
        .send(move |_| {
            first_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, StorageError>(())
        })
        .await
        .expect("first request should clear the reserve");
    let second_count = Arc::clone(&operations);
    let second = handle
        .send(move |_| {
            second_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, StorageError>(())
        })
        .await;

    assert!(matches!(
        second,
        Err(StorageError::CapacityFloor {
            capability: khive_storage::StorageCapability::Sql,
            available_bytes: 100,
            floor_bytes: 100,
            ..
        })
    ));
    assert_eq!(samples.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(operations.load(std::sync::atomic::Ordering::SeqCst), 1);
}
