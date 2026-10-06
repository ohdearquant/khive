#[tokio::test]
async fn in_memory_atomic_unit_pending_future_rolls_back_and_remains_usable() {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE atomic_pending (id INTEGER PRIMARY KEY)")
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let inserted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let op_inserted = Arc::clone(&inserted);
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO atomic_pending VALUES (1)".into(),
                    params: vec![],
                    label: None,
                })
                .await?;
            op_inserted.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<khive_storage::types::StorageResult<Box<dyn Any + Send>>>().await
        })
    });
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(10), bridge.atomic_unit(op)).await;
    assert!(inserted.load(std::sync::atomic::Ordering::SeqCst));
    let error = result
        .expect("in-memory atomic_unit must reject Pending promptly, not await it forever")
        .expect_err("a suspending atomic unit must fail");
    assert!(error.to_string().contains("future suspended"), "{error}");
    {
        let guard = pool.writer().unwrap();
        assert!(guard.conn().is_autocommit());
        let count: i64 = guard
            .conn()
            .query_row("SELECT COUNT(*) FROM atomic_pending", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "SQL before Pending must roll back");
    }
    let next: AtomicUnitOp = Box::new(|writer| {
        Box::pin(async move {
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO atomic_pending VALUES (2)".into(),
                    params: vec![],
                    label: None,
                })
                .await?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        })
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), bridge.atomic_unit(next))
        .await
        .expect("unit admission must remain usable")
        .unwrap();
    let sum: i64 = pool
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT SUM(id) FROM atomic_pending", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sum, 2);
}

// Pause SQLite itself during the first INSERT preparation so tests can
// arrange contention without putting an async wait in an AtomicUnitOp.
fn pause_first_insert(
    pool: &ConnectionPool,
    table: &'static str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let (entered, in_statement) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut entered = Some(entered);
    pool.writer()
        .unwrap()
        .conn()
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action, AuthAction::Insert { table_name } if table_name == table) {
                if let Some(entered) = entered.take() {
                    entered.send(()).unwrap();
                    released
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("test must release the SQLite statement");
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    (in_statement, release)
}

#[tokio::test]
async fn in_memory_atomic_units_serialize_across_bridges() {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE atomic_in_memory (id INTEGER PRIMARY KEY)")
        .unwrap();

    let (in_statement, release) = pause_first_insert(&pool, "atomic_in_memory");
    let ready = Arc::new(tokio::sync::Barrier::new(8));
    let mut jobs = Vec::new();
    for unit in 0..8_i64 {
        let pool = Arc::clone(&pool);
        let ready = Arc::clone(&ready);
        jobs.push(tokio::spawn(async move {
            // Separate bridges must share the pool's transaction budget.
            let bridge = SqlBridge::new(pool, false);
            ready.wait().await;
            let op: AtomicUnitOp = Box::new(move |writer| {
                Box::pin(async move {
                    for row in 0..2 {
                        writer
                            .execute(SqlStatement {
                                sql: "INSERT INTO atomic_in_memory (id) VALUES (?1)".into(),
                                params: vec![SqlValue::Integer(unit * 2 + row)],
                                label: None,
                            })
                            .await?;
                    }
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                })
            });
            bridge.atomic_unit(op).await.map(|_| ())
        }));
    }
    let entered = tokio::time::timeout(std::time::Duration::from_secs(10), in_statement).await;
    if !matches!(&entered, Ok(Ok(()))) {
        drop(release);
        for job in jobs {
            let _ = job.await;
        }
        panic!("atomic unit did not reach its first INSERT: {entered:?}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    release.send(()).unwrap();
    let mut errors = Vec::new();
    for job in jobs {
        if let Err(error) = job.await.unwrap() {
            errors.push(error.to_string());
        }
    }
    assert!(
        errors.is_empty(),
        "atomic units failed: {}",
        errors.join("; ")
    );
    let count: i64 = pool
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM atomic_in_memory", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 16, "all eight two-row units must commit");
}

#[tokio::test]
async fn in_memory_atomic_unit_serializes_with_event_writes() {
    use khive_storage::EventStore as _;

    // Each ordinary event entry point opens a transaction through with_writer.
    for mode in 0..3 {
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                write_queue_enabled: Some(false),
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        {
            let writer = pool.writer().unwrap();
            crate::stores::event::ensure_events_schema(writer.conn()).unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE atomic_event_overlap (id INTEGER PRIMARY KEY)")
                .unwrap();
        }
        let (in_unit, release) = pause_first_insert(&pool, "atomic_event_overlap");
        let bridge = SqlBridge::new(Arc::clone(&pool), false);
        let unit = tokio::spawn(async move {
            let op: AtomicUnitOp = Box::new(move |writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO atomic_event_overlap VALUES (1)".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO atomic_event_overlap VALUES (2)".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                })
            });
            bridge.atomic_unit(op).await.map(|_| ())
        });
        let entered = tokio::time::timeout(std::time::Duration::from_secs(10), in_unit).await;
        if !matches!(&entered, Ok(Ok(()))) {
            drop(release);
            let result = unit.await;
            panic!("atomic unit did not reach its first INSERT: {entered:?}; {result:?}");
        }
        let store = Arc::new(crate::stores::event::SqlEventStore::new_scoped(
            Arc::clone(&pool),
            false,
            "atomic-event",
        ));
        let event = khive_storage::event::Event::new(
            "atomic-event",
            "search",
            khive_types::EventKind::SearchExecuted,
            khive_types::SubstrateKind::Note,
            "agent:test",
        )
        .with_payload(serde_json::json!({"result_kind": "note"}));
        let event_id = event.id;
        let event_store = Arc::clone(&store);
        let mut event_job = tokio::spawn(async move {
            match mode {
                0 => event_store.append_event(event).await,
                1 => event_store.append_events(vec![event]).await.map(|_| ()),
                _ => event_store
                    .append_events_idempotent(vec![event])
                    .await
                    .map(|_| ()),
            }
        });
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut event_job).await;
        let finished_inside_unit = early.is_ok();
        // Release and join both tasks before asserting, including in the RED control.
        release.send(()).unwrap();
        unit.await.unwrap().expect("atomic unit commits both rows");
        let event_result = match early {
            Ok(joined) => joined,
            Err(_) => event_job.await,
        }
        .expect("event task joins");
        assert!(
            event_result.is_ok(),
            "event write mode {mode} overlapped the atomic transaction: {event_result:?}"
        );
        assert!(
            !finished_inside_unit,
            "event write mode {mode} must wait until the atomic unit ends"
        );
        assert!(store.get_event(event_id).await.unwrap().is_some());
        let rows: i64 = pool
            .writer()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM atomic_event_overlap", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2);
    }
}
