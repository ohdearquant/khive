/// Build a bare, file-backed, write-queue-enabled `SqlAccess` handle
/// (sidesteps the process-global `KHIVE_WRITE_QUEUE` env var race — see
/// docs guide ADR-099 D5 notes).
fn write_queue_pool(db_path: std::path::PathBuf) -> Arc<khive_db::ConnectionPool> {
    let pool_cfg = khive_db::PoolConfig {
        path: Some(db_path),
        write_queue_enabled: Some(true),
        ..khive_db::PoolConfig::for_test()
    };
    let pool = Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
    {
        let w_conn = pool.writer().expect("writer");
        w_conn
            .autocommit(|conn| {
                for stmt in &SESSION_SCHEMA_PLAN_STMTS {
                    conn.execute_batch(stmt).expect("session schema stmt");
                }
                Ok(())
            })
            .expect("session schema scope");
    }
    pool
}

/// ADR-099 D5 acceptance: `write_events_and_cursor_on_writer` is
/// suspension-free under `atomic_unit` on the real single-writer path
/// (see docs guide) — a suspending closure would fail `block_on_sync`
/// instead of returning `Ok`.
#[tokio::test]
async fn write_events_and_cursor_is_suspension_free_under_single_writer() {
    let dir = TempDir::new().expect("tempdir");
    let pool = write_queue_pool(dir.path().join("suspend_free.db"));
    let sql: Arc<dyn khive_storage::SqlAccess> =
        Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    pool.writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let events = vec![parse::parse_cc_line(
            r#"{"uuid":"evt-1","sessionId":"suspend-free-session","type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("line must parse")];

    let path = std::path::Path::new("/synthetic/suspend-free.jsonl").to_path_buf();
    let now_us = Utc::now().timestamp_micros();
    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            write_events_and_cursor_on_writer(
                writer,
                &path,
                "claude_code",
                &[],
                &events,
                MirrorWriteProgress {
                    scanned: 1,
                    new_offset: 100,
                    now_us,
                    file_identity: "test-file",
                },
            )
            .await
            .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "test_write_events_and_cursor",
                    e,
                )
            })
        })
    });

    let boxed = sql
        .atomic_unit(op)
        .await
        .expect("a suspension-free closure must not hit block_on_sync's Pending error");
    let stats = *boxed
        .downcast::<MirrorStats>()
        .expect("op must return MirrorStats");

    assert_eq!(stats.inserted, 1, "the one event must be inserted");

    let mut reader = sql.reader().await.expect("reader");
    let row = khive_storage::SqlReader::query_scalar(
        reader.as_mut(),
        SqlStatement {
            sql: "SELECT COUNT(*) FROM sessions".into(),
            params: vec![],
            label: None,
        },
    )
    .await
    .expect("count query")
    .expect("count row");
    match row {
        SqlValue::Integer(1) => {}
        other => panic!("the session row must be committed, got COUNT(*) = {other:?}"),
    }
}

/// ADR-099 D5 acceptance ("single-writer concurrency, mandatory"):
/// session-mirror ingest must route through the shared writer task, not
/// open its own standalone `BEGIN IMMEDIATE` (see docs guide for the
/// queue-depth + occupier-parked-on-oneshot technique).
#[tokio::test]
async fn session_ingest_routes_through_writer_task_when_flag_enabled() {
    let dir = TempDir::new().expect("tempdir");
    let pool = write_queue_pool(dir.path().join("concurrency.db"));
    let sql: Arc<dyn khive_storage::SqlAccess> =
        Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), khive_storage::StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let events = vec![parse::parse_cc_line(
            r#"{"uuid":"evt-concurrency-1","sessionId":"concurrency-session","type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("line must parse")];
    let path = std::path::Path::new("/synthetic/concurrency.jsonl").to_path_buf();
    let now_us = Utc::now().timestamp_micros();
    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            write_events_and_cursor_on_writer(
                writer,
                &path,
                "claude_code",
                &[],
                &events,
                MirrorWriteProgress {
                    scanned: 1,
                    new_offset: 100,
                    now_us,
                    file_identity: "test-file",
                },
            )
            .await
            .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "test_session_ingest_concurrency",
                    e,
                )
            })
        })
    });

    let sql_for_ingest = Arc::clone(&sql);
    let ingest_task = tokio::spawn(async move { sql_for_ingest.atomic_unit(op).await });

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "session-ingest's atomic_unit request never appeared in the writer task's \
             channel while the occupier held the single drain slot — the converted ingest \
             path is not routing through the shared writer task (a standalone `begin_tx` \
             connection would never show up here at all)"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");

    let boxed = ingest_task
        .await
        .expect("ingest task must not panic")
        .expect("ingest atomic_unit must succeed once the occupier releases the slot");
    let stats = *boxed
        .downcast::<MirrorStats>()
        .expect("op must return MirrorStats");
    assert_eq!(stats.inserted, 1, "the ingest event must be committed");
}

/// ADR-099 revert-companion test: the pre-conversion shape (a closure
/// issuing its own `BEGIN IMMEDIATE` inside `atomic_unit`) must fail
/// deterministically with a nested-transaction error — proves the
/// suspension-free assertions above are non-vacuous (see docs guide).
#[tokio::test]
async fn old_shape_manual_begin_immediate_inside_atomic_unit_fails() {
    let dir = TempDir::new().expect("tempdir");
    let pool = write_queue_pool(dir.path().join("old_shape_begin_immediate.db"));
    let sql: Arc<dyn khive_storage::SqlAccess> =
        Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    pool.writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            // `atomic_unit` already has an open transaction around this
            // closure — issuing a second `BEGIN IMMEDIATE` here is
            // exactly the old `begin_tx`-shaped mistake this ADR
            // retires: a caller managing its own transaction control
            // inside a seam that already owns the transaction boundary.
            writer
                .execute(SqlStatement {
                    sql: "BEGIN IMMEDIATE".into(),
                    params: vec![],
                    label: None,
                })
                .await?;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });

    let err = sql.atomic_unit(op).await.expect_err(
        "a closure that issues its own BEGIN IMMEDIATE inside atomic_unit must fail with a \
             nested-transaction error, not silently succeed",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("cannot start a transaction within a transaction"),
        "expected the deterministic nested-transaction failure (SQLite's own message for a \
             second BEGIN issued inside an already-open transaction), got: {msg}"
    );
}
