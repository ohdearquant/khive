use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct CapturingSqlAccess {
    writer_acquisitions: Arc<AtomicUsize>,
    execute_batch_calls: Arc<AtomicUsize>,
    batches: Arc<Mutex<Vec<Vec<SqlStatement>>>>,
    affected: u64,
}

impl CapturingSqlAccess {
    fn new(affected: u64) -> Self {
        Self {
            writer_acquisitions: Arc::new(AtomicUsize::new(0)),
            execute_batch_calls: Arc::new(AtomicUsize::new(0)),
            batches: Arc::new(Mutex::new(Vec::new())),
            affected,
        }
    }
}

struct CapturingWriter {
    execute_batch_calls: Arc<AtomicUsize>,
    batches: Arc<Mutex<Vec<Vec<SqlStatement>>>>,
    affected: u64,
}

#[async_trait::async_trait]
impl khive_storage::SqlReader for CapturingWriter {
    async fn query_row(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Option<khive_storage::SqlRow>> {
        panic!("record_serves must not issue reads")
    }

    async fn query_all(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::SqlRow>> {
        panic!("record_serves must not issue reads")
    }

    async fn query_scalar(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Option<SqlValue>> {
        panic!("record_serves must not issue reads")
    }

    async fn explain(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::SqlRow>> {
        panic!("record_serves must not issue reads")
    }
}

#[async_trait::async_trait]
impl khive_storage::SqlWriter for CapturingWriter {
    async fn execute(&mut self, _statement: SqlStatement) -> khive_storage::StorageResult<u64> {
        panic!("record_serves must use one execute_batch call")
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::StorageResult<u64> {
        self.execute_batch_calls.fetch_add(1, Ordering::SeqCst);
        self.batches.lock().unwrap().push(statements);
        Ok(self.affected)
    }

    async fn execute_script(&mut self, _script: String) -> khive_storage::StorageResult<()> {
        panic!("record_serves must not execute raw scripts")
    }
}

#[async_trait::async_trait]
impl SqlAccess for CapturingSqlAccess {
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        panic!("record_serves must not acquire a reader")
    }

    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        self.writer_acquisitions.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CapturingWriter {
            execute_batch_calls: Arc::clone(&self.execute_batch_calls),
            batches: Arc::clone(&self.batches),
            affected: self.affected,
        }))
    }

    async fn atomic_unit(
        &self,
        _op: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        panic!("record_serves must use the SqlWriter batch seam")
    }
}

fn text_param(statement: &SqlStatement, index: usize) -> &str {
    match &statement.params[index] {
        SqlValue::Text(value) => value,
        other => panic!("parameter {index} must be text, got {other:?}"),
    }
}

#[tokio::test]
async fn record_serves_empty_input_does_not_acquire_writer() {
    let sql = CapturingSqlAccess::new(0);
    let summary = record_serves(
        &sql,
        &[],
        "local",
        "recall",
        Some("profile-a"),
        None,
        None,
        "class-a",
        "raw query",
        7,
        None,
    )
    .await
    .expect("empty batch must succeed");

    assert_eq!(summary, ServeLedgerBatchSummary::default());
    assert_eq!(sql.writer_acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(sql.execute_batch_calls.load(Ordering::SeqCst), 0);
    assert!(sql.batches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn record_serves_uses_one_ordered_batch_and_affected_count() {
    let sql = CapturingSqlAccess::new(2);
    let rows = [
        ServeLedgerInsert {
            id: "row-a".into(),
            target_id: "target-a".into(),
        },
        ServeLedgerInsert {
            id: "row-b".into(),
            target_id: "target-b".into(),
        },
        ServeLedgerInsert {
            id: "row-c".into(),
            target_id: "target-c".into(),
        },
    ];

    let summary = record_serves(
        &sql,
        &rows,
        "namespace-a",
        "recall",
        Some("served-profile"),
        Some("resolved-profile"),
        Some(123),
        "class-a",
        "Literal Raw Query",
        456,
        Some("profile"),
    )
    .await
    .expect("captured batch must succeed");

    assert_eq!(
        summary,
        ServeLedgerBatchSummary {
            written: 2,
            skipped: 1,
        }
    );
    assert_eq!(sql.writer_acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(sql.execute_batch_calls.load(Ordering::SeqCst), 1);

    let batches = sql.batches.lock().unwrap();
    assert_eq!(batches.len(), 1);
    let statements = &batches[0];
    assert_eq!(statements.len(), rows.len());
    // The statement now comes from `sql/brain_serve_ledger_insert.sql`, which wraps
    // its clauses across lines. SQL does not care and neither does this assertion:
    // what it is here to pin is that the batch upserts and swallows the duplicate
    // rather than failing the whole batch, so it reads the clause, not the layout.
    let one_line = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
    for (statement, row) in statements.iter().zip(rows.iter()) {
        assert!(one_line(&statement.sql)
            .ends_with("ON CONFLICT(namespace, target_id, query_class, served_at) DO NOTHING"));
        assert_eq!(
            statement.label.as_deref(),
            Some("brain_serve_ledger_insert")
        );
        assert_eq!(text_param(statement, 0), row.id.as_str());
        assert_eq!(text_param(statement, 1), "namespace-a");
        assert_eq!(text_param(statement, 2), "recall");
        assert_eq!(text_param(statement, 3), "served-profile");
        assert_eq!(text_param(statement, 4), "resolved-profile");
        assert!(matches!(
            statement.params.get(5),
            Some(SqlValue::Integer(123))
        ));
        assert_eq!(text_param(statement, 6), row.target_id.as_str());
        assert_eq!(text_param(statement, 7), "class-a");
        assert_eq!(text_param(statement, 8), "Literal Raw Query");
        assert!(matches!(
            statement.params.get(9),
            Some(SqlValue::Integer(456))
        ));
        assert_eq!(text_param(statement, 10), "profile");
    }
}

#[tokio::test]
async fn record_serves_rolls_back_earlier_rows_on_late_id_collision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("serve-ledger-batch-rollback.db");
    let pool = Arc::new(
        khive_db::ConnectionPool::new(khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: Some(false),
            ..khive_db::PoolConfig::for_test()
        })
        .expect("pool"),
    );
    pool.run_migrations().expect("migrations");
    let sql: Arc<dyn SqlAccess> = Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    record_serve(
        sql.as_ref(),
        "colliding-id",
        "local",
        "recall",
        None,
        None,
        None,
        "preexisting-target",
        "preexisting-class",
        "preexisting query",
        1,
        None,
    )
    .await
    .expect("precondition row must be inserted");

    let rows = [
        ServeLedgerInsert {
            id: "must-roll-back".into(),
            target_id: "fresh-target".into(),
        },
        ServeLedgerInsert {
            id: "colliding-id".into(),
            target_id: "different-target".into(),
        },
    ];
    record_serves(
        sql.as_ref(),
        &rows,
        "local",
        "recall",
        None,
        None,
        None,
        "batch-class",
        "batch query",
        2,
        None,
    )
    .await
    .expect_err("late primary-key collision must fail the whole batch");

    assert!(
        get_serve_row(sql.as_ref(), "must-roll-back")
            .await
            .expect("rollback probe")
            .is_none(),
        "the earlier statement must roll back when a later ID collides"
    );
    assert!(
        get_serve_row(sql.as_ref(), "colliding-id")
            .await
            .expect("precondition probe")
            .is_some(),
        "the pre-existing colliding row must remain"
    );
}

/// Fork C slice 2: proves `record_serve`'s single-row compatibility batch
/// is actually enqueued on the pool's shared `WriterTaskHandle` channel
/// when the write queue is enabled.
///
/// Deliberately NOT a wall-clock/occupier-timing test — see the sibling
/// `fold_gate::tests::fold_gate_apply_routes_through_writer_task_when_flag_enabled`
/// doc comment for why real SQLite file-level locking makes elapsed time
/// alone vacuous here. Instead this reads `WriterTaskHandle::queue_depth`
/// directly while an occupier deterministically holds the writer task's
/// one drain slot open (parked on a oneshot via `blocking_recv`, not a
/// sleep/timing race).
///
/// Deliberately NOT built via a full `KhiveRuntime` + the
/// `KHIVE_WRITE_QUEUE` env var: that env var is process-global and this
/// crate's other tests are not `#[serial]` against it, so a window where
/// it is set here could leak into a concurrently-scheduled test's own
/// `KhiveRuntime::new` and unexpectedly flip its pool's write-queue flag
/// (see `fold_gate::tests::fold_gate_apply_routes_through_writer_task_when_flag_enabled`'s
/// doc comment for the concrete failure this caused before both tests
/// were rewritten to avoid the env var). Constructing the pool directly
/// with `write_queue_enabled: Some(true)` in the config literal, and driving
/// `record_serve` over a bare `SqlBridge` instead of a full
/// `KhiveRuntime`, sidesteps global mutable state entirely.
#[tokio::test]
async fn record_serve_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("serve-ledger-write-queue-routing.db");
    let pool_cfg = khive_db::PoolConfig {
        path: Some(db_path),
        write_queue_enabled: Some(true),
        ..khive_db::PoolConfig::for_test()
    };
    let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
    pool.run_migrations().expect("migrations");
    let sql: std::sync::Arc<dyn SqlAccess> =
        std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

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

    let record_task = tokio::spawn(async move {
        record_serve(
            sql.as_ref(),
            "write-queue-routing-row",
            "local",
            "recall",
            Some("profile-a"),
            None,
            None,
            "write-queue-routing-target",
            "write-queue-routing-class",
            "write queue routing test query",
            1_700_000_000_000_000,
            Some("profile"),
        )
        .await
    });

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
        "record_serve's write request never appeared in the writer task's \
             channel while the occupier held the single drain slot — \
             SqliteWriter::execute_batch is not routing this single-row write \
             through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");

    let inserted = record_task
        .await
        .expect("record_task must not panic")
        .expect("record_serve must succeed once unblocked");
    assert!(inserted, "record_serve must report a fresh row insert");
}

#[test]
fn serve_ledger_sql_err_message_is_prefixed_context_and_error() {
    match sql_err("serve ledger insert", "boom") {
        RuntimeError::Internal(message) => {
            assert_eq!(message, "serve ledger insert: boom");
        }
        other => panic!("expected RuntimeError::Internal, got {other:?}"),
    }
}
