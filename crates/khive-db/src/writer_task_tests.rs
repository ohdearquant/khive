use super::*;
use crate::pool::PoolConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use serial_test::serial;

#[test]
fn begin_error_classification_is_code_based_and_narrow() {
    for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("rendered text is irrelevant".to_string()),
        );
        assert!(matches!(
            writer_task_begin_error(error, Duration::from_millis(175)).without_sqlite_write_stage(),
            StorageError::WriterTaskBusy { timeout_ms: 175 }
        ));
    }

    let structural = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some("database is locked".to_string()),
    );
    assert!(matches!(
        writer_task_begin_error(structural, Duration::from_millis(175)).without_sqlite_write_stage(),
        StorageError::Pool { ref operation, .. } if operation == "writer_task_begin"
    ));
}
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

fn file_pool(path: &std::path::Path) -> ConnectionPool {
    let cfg = PoolConfig {
        path: Some(path.to_path_buf()),
        ..PoolConfig::for_test()
    };
    ConnectionPool::new(cfg).expect("pool open")
}

fn deny_commit_and_rollback(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        // SQLite reports COMMIT as the non-exhaustive Unknown transaction
        // operation in rusqlite 0.40; ROLLBACK has its own variant.
        AuthAction::Transaction {
            operation: TransactionOperation::Unknown | TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

fn deny_commit(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Unknown,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

fn assert_writer_task_terminal_state<T: std::fmt::Debug>(
    result: Result<T, StorageError>,
    expected: WriterTaskRequestState,
) {
    match result
        .as_ref()
        .map_err(StorageError::without_sqlite_write_stage)
    {
        Err(StorageError::WriterTaskTerminated { request_state, .. }) => {
            assert_eq!(*request_state, expected)
        }
        other => panic!("expected WriterTaskTerminated({expected:?}), got {other:?}"),
    }
}

#[test]
fn terminal_native_full_evidence_uses_the_original_error_chain() {
    let extended = rusqlite::ffi::SQLITE_FULL | (3 << 8);
    let native = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(extended),
        Some("synthetic classifier control".into()),
    );
    let wrapped = StorageError::driver(khive_storage::StorageCapability::Sql, "nested", native);
    let error = writer_task_terminated_with_cause(
        WriterTaskRequestState::SideEffectsUnknown,
        &wrapped,
        khive_storage::error::SqliteWriteStage::Statement,
    );
    assert!(
        matches!(error.without_sqlite_write_stage(), StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
            sqlite_full_codes: Some((rusqlite::ffi::SQLITE_FULL, code)),
        } if *code == extended)
    );
    assert!(
        std::error::Error::source(error.without_sqlite_write_stage()).is_none(),
        "evidence must not replay the original sink cause"
    );
    assert!(!error.is_retryable());
    assert_eq!(error.capability(), None);

    for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_IOERR] {
        let native = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("database or disk is full".into()),
        );
        assert!(matches!(
            writer_task_terminated_with_cause(
                WriterTaskRequestState::SideEffectsUnknown,
                &native,
                khive_storage::error::SqliteWriteStage::Statement,
            )
            .without_sqlite_write_stage(),
            StorageError::WriterTaskTerminated {
                sqlite_full_codes: None,
                ..
            }
        ));
    }
    assert!(matches!(
        writer_task_terminated(WriterTaskRequestState::NotStarted),
        StorageError::WriterTaskTerminated {
            sqlite_full_codes: None,
            ..
        }
    ));
}

struct ParkedWake {
    entered: std_mpsc::SyncSender<()>,
    release: Mutex<std_mpsc::Receiver<()>>,
}

impl Wake for ParkedWake {
    fn wake(self: Arc<Self>) {
        self.entered
            .send(())
            .expect("reply sender must rendezvous with the test");
        self.release
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv()
            .expect("test must release the parked reply sender");
    }
}

fn arm_parked_wake<F: Future>(
    mut future: Pin<&mut F>,
) -> (std_mpsc::Receiver<()>, std_mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = std_mpsc::sync_channel(0);
    let (release_tx, release_rx) = std_mpsc::channel();
    let waker = Waker::from(Arc::new(ParkedWake {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    let mut context = Context::from_waker(&waker);
    assert!(
        matches!(future.as_mut().poll(&mut context), Poll::Pending),
        "writer send must remain pending until its operation replies"
    );
    (entered_rx, release_tx)
}

fn poll_ready<F: Future>(mut future: Pin<&mut F>) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("reply wake must make the writer send ready"),
    }
}

fn database_tx_view(pool: &ConnectionPool) -> khive_storage::tx_registry::TxOriginFilter {
    match pool.origin() {
        khive_storage::tx_registry::TxOrigin::Database(identity) => {
            khive_storage::tx_registry::TxOriginFilter::Secondary(identity)
        }
        other => panic!("expected a file-backed database origin, got {other:?}"),
    }
}

async fn wait_for_writer_span_to_close(view: &khive_storage::tx_registry::TxOriginFilter) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while khive_storage::tx_registry::any_open_labeled(view, "writer_task_tx") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer task transaction span must eventually close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(tx_registry)]
async fn writer_task_connection_maintains_rfc3339_expression_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_rfc3339_expression_index.db");
    let pool = file_pool(&path);
    {
        let writer = pool.writer().expect("pooled writer");
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE deadlines(id INTEGER PRIMARY KEY, due TEXT);
                     CREATE INDEX idx_deadlines_strict \
                         ON deadlines(ifnull(khive_rfc3339_strict_key(due), x''));",
            )
            .expect("pooled writer registers the key function");
    }
    let handle = spawn(&pool, 8).expect("writer task spawn");

    let inserted = handle
        .send(|conn| {
            conn.execute(
                "INSERT INTO deadlines(id, due) VALUES (1, '2026-01-01T00:00:00Z')",
                [],
            )
            .map_err(|error| StorageError::Pool {
                operation: "test_insert".into(),
                message: error.to_string(),
            })
        })
        .await
        .expect("the writer task's connection maintains the expression index");
    assert_eq!(inserted, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(tx_registry)]
async fn successful_send_reply_waits_for_writer_tx_deregistration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_success_reply_lifecycle.db");
    let pool = file_pool(&path);
    let view = database_tx_view(&pool);
    let handle = spawn(&pool, 8).expect("writer task spawn");
    let (op_started_tx, op_started_rx) = std_mpsc::sync_channel(0);
    let (op_release_tx, op_release_rx) = std_mpsc::channel();

    let send = handle.send(move |_conn| {
        op_started_tx
            .send(())
            .expect("operation must rendezvous with the test");
        op_release_rx
            .recv()
            .expect("test must release the operation");
        Ok::<_, StorageError>(())
    });
    tokio::pin!(send);
    let (reply_entered_rx, reply_release_tx) = arm_parked_wake(send.as_mut());

    op_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer operation must start");
    op_release_tx.send(()).expect("release writer operation");
    reply_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reply sender must wake the waiting caller");

    let reply = poll_ready(send.as_mut());
    let span_was_open_at_reply =
        khive_storage::tx_registry::any_open_labeled(&view, "writer_task_tx");

    reply_release_tx
        .send(())
        .expect("release parked reply sender");
    wait_for_writer_span_to_close(&view).await;

    reply.expect("committed operation reply");
    assert!(
            !span_was_open_at_reply,
            "a successful caller reply must not become observable while its committed writer_task_tx span remains registered"
        );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(tx_registry)]
async fn begin_failure_reply_waits_for_writer_tx_deregistration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_begin_reply_lifecycle.db");
    // A lock directory of its own keeps this deliberate writer hold off the
    // volume lease the other tests in this process share.
    let cfg = PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        busy_timeout: Duration::from_millis(150),
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    let view = database_tx_view(&pool);
    let handle = spawn(&pool, 8).expect("writer task spawn");
    // An independent SQLite writer does not hold khive's cooperative
    // volume lease, so this reaches the intended BEGIN-busy branch.
    let lock_holder = rusqlite::Connection::open(&path).expect("external writer");
    lock_holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold database write lock");
    let op_ran = Arc::new(AtomicBool::new(false));
    let op_ran_in_request = Arc::clone(&op_ran);

    let send = handle.send(move |_conn| {
        op_ran_in_request.store(true, Ordering::SeqCst);
        Ok::<_, StorageError>(())
    });
    tokio::pin!(send);
    let (reply_entered_rx, reply_release_tx) = arm_parked_wake(send.as_mut());
    reply_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("BEGIN failure must wake the waiting caller");

    let reply = poll_ready(send.as_mut());
    let span_was_open_at_reply =
        khive_storage::tx_registry::any_open_labeled(&view, "writer_task_tx");

    reply_release_tx
        .send(())
        .expect("release parked reply sender");
    wait_for_writer_span_to_close(&view).await;
    lock_holder
        .execute_batch("ROLLBACK")
        .expect("release database write lock");

    assert!(
        matches!(
            reply.as_ref().map_err(StorageError::without_sqlite_write_stage),
            Err(StorageError::WriterTaskBusy { timeout_ms }) if *timeout_ms == 150
        ),
        "expected typed retryable writer-task contention, got {reply:?}"
    );
    assert!(!op_ran.load(Ordering::SeqCst));
    assert!(
            !span_was_open_at_reply,
            "a BEGIN-failure caller reply must not become observable while its writer_task_tx span remains registered"
        );
}

// `#[serial(tx_registry)]`: `run_writer_task` registers a `writer_task_tx`
// handle in the process-wide `tx_registry` singleton for the life of each
// `BEGIN IMMEDIATE`. Tests that observe the registry (the checkpoint
// `tx_age_sweep_*` group) read `tx_registry::oldest()`; an un-serialized
// spawning test here would leak a longer-lived `writer_task_tx` into that
// read and make the sweep name the wrong transaction. Share the key.
#[tokio::test]
#[serial(tx_registry)]
async fn begin_immediate_failure_replies_error_without_running_op() {
    // Real lock contention, not a simulation: an independent SQLite
    // connection holds the database write lock without taking khive's
    // cooperative volume lease. The writer task therefore reaches its
    // dedicated connection's `BEGIN IMMEDIATE` and receives SQLITE_BUSY.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_begin_failure.db");
    // A lock directory of its own keeps this deliberate writer hold off the
    // volume lease the other tests in this process share.
    let cfg = PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        busy_timeout: Duration::from_millis(150),
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }

    let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

    let lock_holder = rusqlite::Connection::open(&path).unwrap();
    lock_holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let op_ran = Arc::new(AtomicBool::new(false));
    let op_ran_clone = Arc::clone(&op_ran);
    let result = handle
        .send(move |conn| {
            op_ran_clone.store(true, Ordering::SeqCst);
            conn.execute("INSERT INTO t (id, v) VALUES (99, 'should-not-land')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert".into(),
                    message: e.to_string(),
                })
        })
        .await;

    assert!(
        matches!(
            result.as_ref().map_err(StorageError::without_sqlite_write_stage),
            Err(StorageError::WriterTaskBusy { timeout_ms }) if *timeout_ms == 150
        ),
        "expected a typed retryable error on contended BEGIN IMMEDIATE, got {result:?}"
    );
    assert!(
        !op_ran.load(Ordering::SeqCst),
        "the request's operation closure must never run when BEGIN \
             IMMEDIATE fails — running it would land a partial write in \
             autocommit mode for a request the caller is told failed"
    );

    let refused = handle.admission_snapshot().unwrap();
    assert_eq!(refused.queue_depth, 0);
    assert_eq!(refused.admission_wait.count, 1);
    assert_eq!(refused.accepted_queue_age.count, 1);
    assert_eq!(
        refused.service.count, 1,
        "BEGIN refusal still reaches execution"
    );
    assert!(
        refused.service.sum >= Duration::from_millis(100),
        "service must include the independently held SQLite lock's 150ms busy budget"
    );
    assert_eq!(refused.saturation_total, 0);

    // Release the contended lock, then verify no row landed from the
    // failed request.
    lock_holder.execute_batch("ROLLBACK").unwrap();
    drop(lock_holder);

    handle
        .send(|conn| {
            conn.execute("INSERT INTO t (id, v) VALUES (100, 'next-request')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert_after_busy".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("transient contention must not retire the writer task");

    let reader = pool.reader().expect("reader");
    let count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id IN (99, 100)", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        count, 1,
        "the failed request must not land, while the next request commits on the same task"
    );
}

// `#[serial(tx_registry)]`: same rationale as
// `begin_immediate_failure_replies_error_without_running_op`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(tx_registry)]
async fn transient_begin_contention_clears_within_budget_and_op_runs_once() {
    // Hold a real SQLite write lock for a fixed, short delay — well
    // inside the configured busy_timeout — so SQLite's own internal
    // busy-handler wait absorbs the contention transparently within a
    // single `BEGIN IMMEDIATE` call. This is the common case the
    // shared retry budget (exercised by
    // `begin_retry_budget_makes_exactly_one_attempt_under_sustained_contention`
    // below) must not regress: recovery inside one busy_timeout window
    // should never touch the Rust-level retry loop or its counters.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_begin_transient_contention.db");
    // A lock directory of its own keeps this deliberate writer hold off the
    // volume lease the other tests in this process share.
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        busy_timeout: Duration::from_millis(500),
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
    // A second khive writer would contend on the cooperative volume lease
    // before SQLite. This fixture needs an independent SQLite writer so it
    // exercises the intended BEGIN busy-handler path.
    let lock_holder = Connection::open(&path).unwrap();
    lock_holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let op_runs = Arc::new(AtomicUsize::new(0));
    let op_runs_in_request = Arc::clone(&op_runs);
    let send_future = handle.send(move |conn| {
        op_runs_in_request.fetch_add(1, Ordering::SeqCst);
        conn.execute("INSERT INTO t (id) VALUES (1)", [])
            .map_err(|error| StorageError::Pool {
                operation: "test_insert_after_transient_contention".into(),
                message: error.to_string(),
            })
    });
    let release_future = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        lock_holder.execute_batch("ROLLBACK").unwrap();
    };
    let (result, ()) = tokio::join!(send_future, release_future);

    assert_eq!(
        result.expect("BEGIN IMMEDIATE succeeds once the transient lock clears"),
        1
    );
    assert_eq!(
        op_runs.load(Ordering::SeqCst),
        1,
        "the FnOnce request closure must execute exactly once"
    );

    let settled = pool.writer_acquisition_snapshot();
    assert_eq!(
        settled.writer_task_begin_busy, 0,
        "contention absorbed inside SQLite's own busy_timeout wait must never \
             surface as a Rust-level refusal"
    );
    assert_eq!(settled.writer_task_begin_busy_absorbed, 0);
    let reader = pool.reader().unwrap();
    let rows: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "exactly one closure execution commits one row");
}

include!("writer_task_admission_retry_tests.rs");

// `#[serial(tx_registry)]`: shares the key with the checkpoint
// `tx_age_sweep_*` tests — see the note on
// `begin_immediate_failure_replies_error_without_running_op`.
#[tokio::test]
#[serial(tx_registry)]
async fn writer_task_executes_op_and_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_commit.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }

    let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

    let affected = handle
        .send(|conn| {
            conn.execute("INSERT INTO t (id, v) VALUES (1, 'hello')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("op should succeed");
    assert_eq!(affected, 1);

    // Verify the write actually committed to the shared file — read it
    // back via a fresh pooled reader connection, not the writer task's
    // own connection.
    let reader = pool.reader().expect("reader");
    let v: String = reader
        .conn()
        .query_row("SELECT v FROM t WHERE id = 1", [], |row| row.get(0))
        .expect("row must be committed and visible to a reader");
    assert_eq!(v, "hello");

    let counters = pool.writer_acquisition_snapshot();
    assert_eq!(counters.acquisitions, 2);
    assert_eq!(counters.pooled_acquisitions, 1);
    assert_eq!(counters.standalone_acquisitions, 0);
    assert_eq!(counters.writer_task_acquisitions, 1);
    assert_eq!(counters.timeouts, 0);
}

#[tokio::test]
async fn writer_task_connection_follows_checkpoint_ownership_claim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_autocheckpoint.db");
    let pool = file_pool(&path);
    // The pool-registered handle (not a directly spawned side task):
    // `propagate_checkpoint_claim_to_writer_task` reaches exactly this
    // task's connection.
    let handle = pool
        .writer_task_handle()
        .expect("writer task should spawn")
        .expect("file-backed pool resolves the write queue on");

    let read_pages = |handle: &WriterTaskHandle| {
        let handle = handle.clone();
        async move {
            handle
                .send_top_level(|conn| {
                    conn.pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, u32>(0))
                        .map_err(|e| StorageError::Pool {
                            operation: "test_wal_autocheckpoint".into(),
                            message: e.to_string(),
                        })
                })
                .await
                .expect("query writer-task connection pragma")
        }
    };

    // Spawned before any claim: the task's long-lived connection keeps
    // the bounded fallback.
    assert_eq!(
        read_pages(&handle).await,
        crate::pool::FALLBACK_WAL_AUTOCHECKPOINT_PAGES
    );

    // After the claim propagates, the same connection is flipped to the
    // dedicated-owner setting without reopening.
    pool.claim_checkpoint_ownership().expect("claim ownership");
    pool.propagate_checkpoint_claim_to_writer_task()
        .await
        .expect("propagate claim to the running writer task");
    assert_eq!(read_pages(&handle).await, 0);
}

#[test]
fn spawn_fails_on_in_memory_pool() {
    // In-memory pools have no standalone-connection support
    // (the infrastructure-only standalone open) — `spawn` must surface
    // that as an error rather than panicking. Deliberately a plain
    // `#[test]` (no Tokio runtime): `spawn` fails before it ever reaches
    // `tokio::spawn`, so no runtime is required for this path.
    let cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    let result = spawn(&pool, 8);
    assert!(
        result.is_err(),
        "in-memory pools must reject spawn, not panic"
    );
}

include!("writer_task_queue_capacity_tests.rs");

#[tokio::test]
#[serial(tx_registry)]
async fn operation_failure_with_successful_rollback_reports_finality_once_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_operation_rollback.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    let handle = spawn(&pool, 8).expect("writer task spawn");

    let executions = Arc::new(AtomicUsize::new(0));
    let executions_in_op = Arc::clone(&executions);
    let original_error = handle
        .send(move |conn| -> Result<(), StorageError> {
            executions_in_op.fetch_add(1, Ordering::SeqCst);
            conn.execute("INSERT INTO t (id, v) VALUES (1, 'rolled-back')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_operation_error_insert".into(),
                    message: e.to_string(),
                })?;
            Err(StorageError::Internal(
                "intentional operation failure".into(),
            ))
        })
        .await;
    match &original_error {
            Err(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source,
            }) => assert!(
                matches!(source.as_ref(), StorageError::Internal(message)
                    if message == "intentional operation failure"),
                "the proven-rollback wrapper must retain the typed operation error: {source:?}"
            ),
            other => panic!(
                "a confirmed rollback must carry TransactionRolledBack and preserve the operation error, got {other:?}"
            ),
        }
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "finality propagation must not replay the request closure"
    );

    let affected = handle
        .send(|conn| {
            conn.execute("INSERT INTO t (id, v) VALUES (2, 'committed')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_operation_error_followup_insert".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("the writer must continue after a confirmed rollback");
    assert_eq!(affected, 1);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "serving a follow-up request must not replay the rolled-back closure"
    );

    let reader = pool.reader().expect("reader");
    let rolled_back: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    let committed: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id = 2", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rolled_back, 0);
    assert_eq!(committed, 1);
}

#[tokio::test]
#[serial(tx_registry)]
async fn commit_failure_with_successful_rollback_reports_finality_once_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_commit_rollback.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    let handle = spawn(&pool, 8).expect("writer task spawn");

    let executions = Arc::new(AtomicUsize::new(0));
    let executions_in_op = Arc::clone(&executions);
    let commit_error = handle
        .send(move |conn| -> Result<usize, StorageError> {
            executions_in_op.fetch_add(1, Ordering::SeqCst);
            let affected = conn
                .execute("INSERT INTO t (id, v) VALUES (1, 'rolled-back')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_commit_error_insert".into(),
                    message: e.to_string(),
                })?;
            conn.authorizer(Some(deny_commit))
                .map_err(|e| StorageError::Pool {
                    operation: "test_install_authorizer".into(),
                    message: e.to_string(),
                })?;
            Ok(affected)
        })
        .await;
    match &commit_error {
            Err(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source,
            }) => assert!(
                matches!(source.without_sqlite_write_stage(), StorageError::Pool { operation, .. }
                    if operation == "writer_task_commit"),
                "the proven-rollback wrapper must retain the typed COMMIT error: {source:?}"
            ),
            other => panic!(
                "a confirmed rollback must carry TransactionRolledBack and preserve the COMMIT error, got {other:?}"
            ),
        }
    assert!(
        commit_error
            .as_ref()
            .expect_err("COMMIT must be denied")
            .is_retryable(),
        "the existing retryable commit-error contract must remain unchanged after a \
             confirmed rollback"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "finality propagation must not replay the request closure"
    );

    let affected = handle
        .send(|conn| {
            conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .map_err(|e| StorageError::Pool {
                    operation: "test_remove_authorizer".into(),
                    message: e.to_string(),
                })?;
            conn.execute("INSERT INTO t (id, v) VALUES (2, 'committed')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_commit_error_followup_insert".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("the writer must continue after the failed COMMIT is rolled back");
    assert_eq!(affected, 1);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "serving a follow-up request must not replay the rolled-back closure"
    );

    let reader = pool.reader().expect("reader");
    let rolled_back: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    let committed: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id = 2", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rolled_back, 0);
    assert_eq!(committed, 1);
}

#[test]
fn top_level_request_returning_with_open_transaction_reports_side_effects_unknown() {
    let conn = Connection::open_in_memory().expect("in-memory connection");
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .unwrap();
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let request = WriteRequest {
        op: Box::new(|conn| -> Result<usize, StorageError> {
            conn.execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| StorageError::Pool {
                    operation: "test_top_level_begin".into(),
                    message: e.to_string(),
                })?;
            conn.execute("INSERT INTO t (id) VALUES (1)", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_top_level_insert".into(),
                    message: e.to_string(),
                })
        }),
        reply: reply_tx,
        top_level: true,
        checkpoint_bypass: false,
        vacuum_copy_headroom: false,
        telemetry: WriteTelemetry::new(None, "test".to_string(), 0, None, SubmitLabels::None),
        admission: None,
    };

    let terminal_state = sealed::Sealed::execute_and_reply_top_level_reporting_terminal(
        Box::new(request),
        &conn,
        Duration::ZERO,
    );
    assert_eq!(
        terminal_state,
        Some(WriterTaskRequestState::SideEffectsUnknown)
    );
    let reply = reply_rx
        .try_recv()
        .expect("active request must receive a typed terminal reply");
    assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);
    assert!(
        !conn.is_autocommit(),
        "the fixture must prove the post-request autocommit check observed an open transaction"
    );
}

#[test]
fn commit_failure_with_failed_rollback_reports_side_effects_unknown() {
    let conn = Connection::open_in_memory().expect("in-memory connection");
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
        .unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let executions_in_op = Arc::clone(&executions);
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let request = WriteRequest {
        op: Box::new(move |conn| -> Result<usize, StorageError> {
            executions_in_op.fetch_add(1, Ordering::SeqCst);
            let affected = conn
                .execute("INSERT INTO t (id) VALUES (1)", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_insert_before_commit_failure".into(),
                    message: e.to_string(),
                })?;
            conn.authorizer(Some(deny_commit_and_rollback))
                .map_err(|e| StorageError::Pool {
                    operation: "test_install_authorizer".into(),
                    message: e.to_string(),
                })?;
            Ok(affected)
        }),
        reply: reply_tx,
        top_level: false,
        checkpoint_bypass: false,
        vacuum_copy_headroom: false,
        telemetry: WriteTelemetry::new(None, "test".to_string(), 0, None, SubmitLabels::None),
        admission: None,
    };

    let terminal_state = sealed::Sealed::execute_and_reply_reporting_terminal(
        Box::new(request),
        &conn,
        None,
        Duration::ZERO,
        Duration::ZERO,
    );
    assert_eq!(
        terminal_state,
        Some(WriterTaskRequestState::SideEffectsUnknown)
    );
    let reply = reply_rx
        .try_recv()
        .expect("active request must receive a typed terminal reply");
    let evidence = reply.as_ref().unwrap_err().sqlite_write_failure().unwrap();
    assert_eq!(
        evidence.stage,
        khive_storage::error::SqliteWriteStage::Commit
    );
    assert_eq!(evidence.primary_code, rusqlite::ffi::SQLITE_AUTH);
    assert_eq!(evidence.extended_code, rusqlite::ffi::SQLITE_AUTH);
    assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        !conn.is_autocommit(),
        "the denied COMMIT and ROLLBACK must leave the test connection poisoned"
    );
}

#[tokio::test]
#[serial(tx_registry)]
async fn poisoned_connection_retires_before_queued_top_level_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_rollback_poison.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    let handle = spawn(&pool, 8).expect("writer task spawn");
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();

    let active = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .send(move |conn| -> Result<usize, StorageError> {
                    let affected = conn
                        .execute("INSERT INTO t (id, v) VALUES (1, 'active')", [])
                        .map_err(|e| StorageError::Pool {
                            operation: "test_active_insert".into(),
                            message: e.to_string(),
                        })?;
                    conn.authorizer(Some(deny_commit_and_rollback))
                        .map_err(|e| StorageError::Pool {
                            operation: "test_install_authorizer".into(),
                            message: e.to_string(),
                        })?;
                    let _ = started_tx.send(());
                    release_rx.recv().expect("test must release active op");
                    Ok(affected)
                })
                .await
        }
    });

    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("active request did not start")
        .expect("active request dropped its start signal");

    let queued_ran = Arc::new(AtomicBool::new(false));
    let queued_ran_in_op = Arc::clone(&queued_ran);
    let queued_top_level = handle
        .enqueue_inner(
            move |conn| {
                queued_ran_in_op.store(true, Ordering::SeqCst);
                conn.execute("INSERT INTO t (id, v) VALUES (2, 'queued')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_queued_top_level_insert".into(),
                        message: e.to_string(),
                    })
            },
            true,
            false,
            false,
            SubmitLabels::None,
        )
        .await
        .expect("top-level request must queue behind active request");
    release_tx.send(()).expect("release active op");

    let active_result = tokio::time::timeout(Duration::from_secs(5), active)
        .await
        .expect("active caller hung after rollback failure")
        .expect("active caller task join");
    assert_writer_task_terminal_state(active_result, WriterTaskRequestState::SideEffectsUnknown);

    let queued_result = tokio::time::timeout(Duration::from_secs(5), queued_top_level)
        .await
        .expect("queued top-level caller hung after terminal failure")
        .expect("terminal drain must preserve queued typed reply");
    assert_writer_task_terminal_state(queued_result, WriterTaskRequestState::NotStarted);
    assert!(
        !queued_ran.load(Ordering::SeqCst),
        "a top-level request must never run on the poisoned connection"
    );

    let future_ran = Arc::new(AtomicBool::new(false));
    let future_ran_in_op = Arc::clone(&future_ran);
    let future_result = handle
        .send_top_level(move |_conn| {
            future_ran_in_op.store(true, Ordering::SeqCst);
            Ok::<(), StorageError>(())
        })
        .await;
    assert_writer_task_terminal_state(future_result, WriterTaskRequestState::NotStarted);
    assert!(!future_ran.load(Ordering::SeqCst));
    let metrics = handle.admission_snapshot().unwrap();
    assert_eq!(metrics.queue_depth, 0);
    assert_eq!(metrics.admission_wait.count, 2);
    assert_eq!(metrics.accepted_queue_age.count, 1);
    assert_eq!(metrics.service.count, 1);
    assert_eq!(metrics.saturation_total, 0);
}

#[test]
fn operation_failure_with_failed_rollback_reports_side_effects_unknown() {
    let conn = Connection::open_in_memory().expect("in-memory connection");
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
        .unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let executions_in_op = Arc::clone(&executions);
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let request = WriteRequest {
        op: Box::new(move |conn| -> Result<(), StorageError> {
            executions_in_op.fetch_add(1, Ordering::SeqCst);
            conn.authorizer(Some(deny_rollback))
                .map_err(|e| StorageError::Pool {
                    operation: "test_install_authorizer".into(),
                    message: e.to_string(),
                })?;
            Err(StorageError::Internal(
                "intentional operation failure before denied rollback".into(),
            ))
        }),
        reply: reply_tx,
        top_level: false,
        checkpoint_bypass: false,
        vacuum_copy_headroom: false,
        telemetry: WriteTelemetry::new(None, "test".to_string(), 0, None, SubmitLabels::None),
        admission: None,
    };

    let terminal_state = sealed::Sealed::execute_and_reply_reporting_terminal(
        Box::new(request),
        &conn,
        None,
        Duration::ZERO,
        Duration::ZERO,
    );
    assert_eq!(
        terminal_state,
        Some(WriterTaskRequestState::SideEffectsUnknown)
    );
    let reply = reply_rx
        .try_recv()
        .expect("active request must receive a typed terminal reply");
    assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        !conn.is_autocommit(),
        "the denied ROLLBACK must leave the test connection poisoned"
    );
}

#[test]
fn wrapped_panic_with_failed_rollback_reports_side_effects_unknown() {
    let conn = Connection::open_in_memory().expect("in-memory connection");
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
        .unwrap();
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let request = WriteRequest {
        op: Box::new(|conn| -> Result<(), StorageError> {
            // Deliberately violate WriteOp's no-COMMIT contract to create
            // the otherwise rare but reachable rollback-failure state.
            // The row commits before the panic, so reporting a successful
            // rollback here would be dangerously false.
            conn.execute_batch("INSERT INTO t (id) VALUES (1); COMMIT")
                .map_err(|e| StorageError::Pool {
                    operation: "test_force_rollback_failure".into(),
                    message: e.to_string(),
                })?;
            panic!("intentional panic after illicit commit");
        }),
        reply: reply_tx,
        top_level: false,
        checkpoint_bypass: false,
        vacuum_copy_headroom: false,
        telemetry: WriteTelemetry::new(None, "test".to_string(), 0, None, SubmitLabels::None),
        admission: None,
    };

    let terminal_state = sealed::Sealed::execute_and_reply_reporting_terminal(
        Box::new(request),
        &conn,
        None,
        Duration::ZERO,
        Duration::ZERO,
    );
    assert_eq!(
        terminal_state,
        Some(WriterTaskRequestState::SideEffectsUnknown)
    );
    let reply = reply_rx
        .try_recv()
        .expect("active request must receive a typed terminal reply");
    assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "the fixture's committed side effect proves why the state must be unknown"
    );
}

// `#[serial(tx_registry)]`: the active request holds a registered
// `writer_task_tx` until its panic is caught and rolled back. Share the
// process-wide registry key with the other writer-task transaction tests.
#[tokio::test]
#[serial(tx_registry)]
async fn wrapped_panic_rolls_back_and_terminally_fails_queue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_wrapped_panic.db");
    let cfg = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_queue_capacity: 8,
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }

    // Use the pool-owned OnceLock path rather than `spawn` directly so
    // the test also proves a terminal task is never silently replaced.
    let handle = pool
        .writer_task_handle()
        .expect("writer task lookup")
        .expect("file-backed queued pool must spawn its writer task");
    assert_eq!(pool.writer_task_spawn_count(), 1);

    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let active = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .send(move |conn| -> Result<usize, StorageError> {
                    conn.execute("INSERT INTO t (id, v) VALUES (1, 'active')", [])
                        .map_err(|e| StorageError::Pool {
                            operation: "test_active_insert".into(),
                            message: e.to_string(),
                        })?;
                    let _ = started_tx.send(());
                    release_rx.recv().expect("test must release active op");
                    panic!("intentional wrapped writer request panic");
                })
                .await
        }
    });

    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("active request did not start")
        .expect("active request dropped its start signal");

    let queued_one_ran = Arc::new(AtomicBool::new(false));
    let queued_one_ran_in_op = Arc::clone(&queued_one_ran);
    let queued_one = handle
        .enqueue(move |conn| {
            queued_one_ran_in_op.store(true, Ordering::SeqCst);
            conn.execute("INSERT INTO t (id, v) VALUES (2, 'queued-one')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_queued_one_insert".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("first queued request must be accepted");

    let queued_two_ran = Arc::new(AtomicBool::new(false));
    let queued_two_ran_in_op = Arc::clone(&queued_two_ran);
    let queued_two = handle
        .enqueue(move |_conn| {
            queued_two_ran_in_op.store(true, Ordering::SeqCst);
            Ok::<String, StorageError>("queued-two-ran".to_string())
        })
        .await
        .expect("second queued request must be accepted");

    assert_eq!(
        handle.queue_depth(),
        2,
        "both heterogeneous requests must be buffered behind the active op"
    );
    release_tx.send(()).expect("release active op");

    let active_result = tokio::time::timeout(Duration::from_secs(5), active)
        .await
        .expect("active caller hung after panic")
        .expect("active caller task join");
    assert_writer_task_terminal_state(active_result, WriterTaskRequestState::TransactionRolledBack);

    let queued_one_result = tokio::time::timeout(Duration::from_secs(5), queued_one)
        .await
        .expect("first queued caller hung after terminal failure")
        .expect("terminal drain must preserve first typed reply");
    assert_writer_task_terminal_state(queued_one_result, WriterTaskRequestState::NotStarted);

    let queued_two_result = tokio::time::timeout(Duration::from_secs(5), queued_two)
        .await
        .expect("second queued caller hung after terminal failure")
        .expect("terminal drain must preserve second typed reply");
    assert_writer_task_terminal_state(queued_two_result, WriterTaskRequestState::NotStarted);
    assert!(!queued_one_ran.load(Ordering::SeqCst));
    assert!(!queued_two_ran.load(Ordering::SeqCst));

    let future_ran = Arc::new(AtomicBool::new(false));
    let future_ran_in_op = Arc::clone(&future_ran);
    let future_result = handle
        .send(move |_conn| {
            future_ran_in_op.store(true, Ordering::SeqCst);
            Ok::<(), StorageError>(())
        })
        .await;
    assert_writer_task_terminal_state(future_result, WriterTaskRequestState::NotStarted);
    assert!(!future_ran.load(Ordering::SeqCst));

    let cached_after_failure = pool
        .writer_task_handle()
        .expect("cached writer task lookup")
        .expect("pool retains its terminal handle");
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "a terminal writer task must not be restarted behind callers' backs"
    );
    let cached_result = cached_after_failure
        .send(|_conn| Ok::<(), StorageError>(()))
        .await;
    assert_writer_task_terminal_state(cached_result, WriterTaskRequestState::NotStarted);

    let reader = pool.reader().expect("reader");
    let count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "the active transaction must be rolled back and queued ops must never run"
    );
}

#[tokio::test]
async fn top_level_panic_reports_unknown_and_fails_queue_without_running_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_top_level_panic.db");
    let pool = file_pool(&path);
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
    }
    let handle = spawn(&pool, 8).expect("writer task spawn");

    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let active = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .send_top_level(move |conn| -> Result<usize, StorageError> {
                    conn.execute("INSERT INTO t (id, v) VALUES (10, 'autocommitted')", [])
                        .map_err(|e| StorageError::Pool {
                            operation: "test_top_level_insert".into(),
                            message: e.to_string(),
                        })?;
                    let _ = started_tx.send(());
                    release_rx.recv().expect("test must release top-level op");
                    panic!("intentional top-level writer request panic");
                })
                .await
        }
    });

    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("top-level request did not start")
        .expect("top-level request dropped its start signal");

    let queued_ran = Arc::new(AtomicBool::new(false));
    let queued_ran_in_op = Arc::clone(&queued_ran);
    let queued = handle
        .enqueue(move |conn| {
            queued_ran_in_op.store(true, Ordering::SeqCst);
            conn.execute("INSERT INTO t (id, v) VALUES (11, 'queued')", [])
                .map_err(|e| StorageError::Pool {
                    operation: "test_top_level_queued_insert".into(),
                    message: e.to_string(),
                })
        })
        .await
        .expect("queued request must be accepted");
    assert_eq!(handle.queue_depth(), 1);
    release_tx.send(()).expect("release top-level op");

    let active_result = tokio::time::timeout(Duration::from_secs(5), active)
        .await
        .expect("top-level caller hung after panic")
        .expect("top-level caller task join");
    assert_writer_task_terminal_state(active_result, WriterTaskRequestState::SideEffectsUnknown);

    let queued_result = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("queued caller hung after top-level panic")
        .expect("terminal drain must preserve queued typed reply");
    assert_writer_task_terminal_state(queued_result, WriterTaskRequestState::NotStarted);
    assert!(!queued_ran.load(Ordering::SeqCst));

    let reader = pool.reader().expect("reader");
    let active_count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id = 10", [], |row| row.get(0))
        .unwrap();
    let queued_count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM t WHERE id = 11", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        active_count, 1,
        "the completed top-level statement autocommits before the panic"
    );
    assert_eq!(queued_count, 0, "the queued request must never run");
    let metrics = handle.admission_snapshot().unwrap();
    assert_eq!(metrics.queue_depth, 0);
    assert_eq!(metrics.admission_wait.count, 2);
    assert_eq!(metrics.accepted_queue_age.count, 1);
    assert_eq!(metrics.service.count, 1);
    assert_eq!(metrics.saturation_total, 0);
}

#[tokio::test]
async fn closed_receiver_rejects_all_send_surfaces_as_not_started() {
    // Simulates the writer task having terminated: its `rx` is gone, so
    // every send surface must fail deterministically without running the
    // supplied operation.
    let (tx, rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(4);
    drop(rx);

    let handle = WriterTaskHandle {
        tx,
        backend_key: None,
        db: "test".to_string(),
        slow_write_threshold: None,
        enqueue_timeout: Duration::from_secs(5),
        admission_metrics: WriterAdmissionMetrics::new(),
    };
    let send_result = handle.send(|_conn| Ok::<(), StorageError>(())).await;
    assert_writer_task_terminal_state(send_result, WriterTaskRequestState::NotStarted);

    let timed_result = handle
        .send_with_timeout(|_conn| Ok::<(), StorageError>(()), Duration::from_secs(1))
        .await;
    assert_writer_task_terminal_state(timed_result, WriterTaskRequestState::NotStarted);

    let top_level_result = handle
        .send_top_level(|_conn| Ok::<(), StorageError>(()))
        .await;
    assert_writer_task_terminal_state(top_level_result, WriterTaskRequestState::NotStarted);
}

#[tokio::test]
async fn accepted_request_lost_reply_is_side_effects_unknown() {
    // A request can be accepted before an unexpected receiver/task loss.
    // The handle cannot prove whether its closure ran, so the oneshot
    // fallback must conservatively report SideEffectsUnknown.
    let (tx, mut rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
    let handle = WriterTaskHandle {
        tx,
        backend_key: None,
        db: "test".to_string(),
        slow_write_threshold: None,
        enqueue_timeout: Duration::from_secs(5),
        admission_metrics: WriterAdmissionMetrics::new(),
    };
    let request_ran = Arc::new(AtomicBool::new(false));
    let request_ran_in_op = Arc::clone(&request_ran);

    let dropper = tokio::spawn(async move {
        let request = rx.recv().await.expect("request must be accepted");
        drop(request);
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        handle.send(move |_conn| {
            request_ran_in_op.store(true, Ordering::SeqCst);
            Ok::<(), StorageError>(())
        }),
    )
    .await
    .expect("caller hung after accepted request was dropped");
    dropper.await.expect("dropper task join");

    assert_writer_task_terminal_state(result, WriterTaskRequestState::SideEffectsUnknown);
    assert!(!request_ran.load(Ordering::SeqCst));
}

/// #1849: writer telemetry uses the same canonical OS-path identity as
/// the pool. A lossy display string would merge these distinct backends.
#[cfg(unix)]
#[test]
fn writer_stage_backend_key_preserves_non_utf8_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let path_a =
        std::path::PathBuf::from(OsString::from_vec(b"/tmp/khive-writer-\x80.db".to_vec()));
    let path_b =
        std::path::PathBuf::from(OsString::from_vec(b"/tmp/khive-writer-\x81.db".to_vec()));
    assert_eq!(
        path_a.display().to_string(),
        path_b.display().to_string(),
        "fixture must reproduce the lossy display-label collision"
    );
    assert_ne!(
        writer_db_key_from_path(Some(&path_a)),
        writer_db_key_from_path(Some(&path_b)),
        "backend keys must retain the canonical path's exact OS bytes"
    );
}

// A clock belongs to one request, so parallel tests cannot affect its samples.
struct ControlledWriterClock {
    origin: std::time::Instant,
    micros: std::sync::atomic::AtomicU64,
}

impl TestWriterStageClock for ControlledWriterClock {
    fn now(&self) -> std::time::Instant {
        self.origin + Duration::from_micros(self.micros.load(Ordering::SeqCst))
    }

    fn at_commit(&self) {
        self.micros.fetch_add(11_000, Ordering::SeqCst);
    }
}

#[test]
fn writer_stage_sample_attributes_body_and_commit_at_boundaries() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Used only as a unique observation key; no database is opened at this path.
    let key = Some(dir.path().join("observation"));
    let clock = Arc::new(ControlledWriterClock {
        origin: std::time::Instant::now(),
        micros: std::sync::atomic::AtomicU64::new(0),
    });
    let body_clock = Arc::clone(&clock);
    let mut telemetry =
        WriteTelemetry::new(key.clone(), "test".into(), 0, None, SubmitLabels::None);
    telemetry.test_clock = Some(clock);
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let request = WriteRequest {
        op: Box::new(move |conn| {
            body_clock.micros.fetch_add(37_000, Ordering::SeqCst);
            conn.execute("INSERT INTO t VALUES (1)", [])
                .map_err(|e| StorageError::Pool {
                    operation: "controlled_body".into(),
                    message: e.to_string(),
                })
        }),
        reply: reply_tx,
        top_level: false,
        checkpoint_bypass: false,
        vacuum_copy_headroom: false,
        telemetry,
        admission: None,
    };
    let terminal = sealed::Sealed::execute_and_reply_reporting_terminal(
        Box::new(request),
        &conn,
        None,
        Duration::from_micros(7_000),
        Duration::from_micros(13_000),
    );
    assert!(terminal.is_none());
    assert_eq!(reply_rx.try_recv().unwrap().unwrap(), 1);
    assert!(conn.is_autocommit());
    let sample = writer_stage_observations()
        .lock()
        .unwrap()
        .remove(&key)
        .unwrap();
    assert_eq!(sample.queue_wait_micros, 7_000);
    assert_eq!(sample.transaction_acquire_micros, 13_000);
    assert_eq!(sample.body_micros, 37_000);
    assert_eq!(sample.commit_micros, 11_000);
    assert!(sample.observed_at_unix_ms > 0);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn wrapped_transaction_synthetic_commit_and_rollback() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
        .unwrap();
    let (result, terminal) = execute_wrapped_transaction(&conn, "test_commit", |conn| {
        conn.execute("INSERT INTO t VALUES (1)", [])
            .map_err(|e| StorageError::Pool {
                operation: "test_insert".into(),
                message: e.to_string(),
            })
    });
    assert_eq!(result.unwrap(), 1);
    assert!(terminal.is_none());
    assert!(conn.is_autocommit());
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (result, terminal) = execute_wrapped_transaction(&conn, "test_commit", |conn| {
        conn.execute_batch("INSERT INTO t VALUES (2)").unwrap();
        Err::<(), _>(StorageError::Internal("synthetic failure".into()))
    });
    assert!(result.is_err());
    assert!(terminal.is_none());
    assert!(conn.is_autocommit());
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

/// #1849: bounded-channel queue wait ends when the drain loop receives
/// the request. Saturation in Tokio's blocking pool happens after that
/// boundary and must remain in `total - named_stages`, never be relabeled
/// as writer-queue contention.
#[test]
fn writer_queue_wait_excludes_blocking_pool_scheduling_delay() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("test runtime");

    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_dequeue_boundary.db");
        let pool = file_pool(&path);
        let handle = spawn(&pool, 8).unwrap();

        let (blocker_started_tx, blocker_started_rx) = std_mpsc::sync_channel(0);
        let (release_blocker_tx, release_blocker_rx) = std_mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            blocker_started_tx.send(()).unwrap();
            release_blocker_rx.recv().unwrap();
        });
        blocker_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sole blocking worker must be occupied");

        let reply = handle
            .enqueue(|_conn| Ok::<(), StorageError>(()))
            .await
            .expect("request must enter the bounded writer channel");
        let dequeue_deadline = Instant::now() + Duration::from_secs(1);
        while handle.queue_depth() != 0
            || handle
                .admission_snapshot()
                .unwrap()
                .accepted_queue_age
                .count
                == 0
        {
            assert!(
                Instant::now() < dequeue_deadline,
                "writer drain never dequeued the accepted request"
            );
            tokio::task::yield_now().await;
        }

        let before_delay = handle.admission_snapshot().unwrap();
        assert_eq!(before_delay.accepted_queue_age.count, 1);
        assert_eq!(before_delay.service.count, 0);
        let service_hold_started = Instant::now();
        let scheduling_delay = Duration::from_millis(150);
        tokio::time::sleep(scheduling_delay).await;
        release_blocker_tx.send(()).unwrap();
        blocker.await.unwrap();
        reply.await.unwrap().unwrap();

        let metrics = handle.admission_snapshot().unwrap();
        assert_eq!(metrics.service.count, 1);
        assert!(metrics.service.sum >= scheduling_delay);
        assert!(service_hold_started.elapsed() >= scheduling_delay);
        assert_eq!(metrics.accepted_queue_age, before_delay.accepted_queue_age);
        let sample = last_writer_stage_observation(&pool).expect("writer stage sample");
        assert!(
            sample.total_micros.saturating_sub(sample.queue_wait_micros) >= 100_000,
            "the post-dequeue blocking-pool delay must not inflate queue_wait: {sample:?}"
        );
    });
}

// `#[serial(tx_registry)]`: the active request holds a registered
// `writer_task_tx` before its terminal failure, sharing the process-wide
// registry key with the other writer-task transaction tests.
#[tokio::test]
#[serial(tx_registry)]
async fn writer_task_failure_counters_are_acquisition_site_exact() {
    // Case 1: a request that reaches the seam, panics, and rolls back
    // cleanly must move `writer_task_request_failures` by exactly one —
    // never `writer_task_side_effects_unknown` — and the requests that
    // never dequeued into the seam (failed only because the queue closed
    // behind them) must move neither counter.
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_failure_counters_rollback.db");
        let pool = file_pool(&path);
        let handle = spawn(&pool, 8).expect("writer task should spawn");

        let before = pool.writer_acquisition_snapshot();
        assert_eq!(before.writer_task_request_failures, 0);
        assert_eq!(before.writer_task_side_effects_unknown, 0);

        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let active = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .send(move |_conn| -> Result<(), StorageError> {
                        let _ = started_tx.send(());
                        release_rx.recv().expect("test must release active op");
                        panic!("intentional rollback-clean panic for counter test");
                    })
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("active request did not start")
            .expect("active request dropped its start signal");

        let queued = handle
            .enqueue(|_conn| Ok::<(), StorageError>(()))
            .await
            .expect("second request must queue behind the active one");

        release_tx.send(()).expect("release active op");
        let active_result = tokio::time::timeout(Duration::from_secs(5), active)
            .await
            .expect("active caller hung after panic")
            .expect("active caller task join");
        assert_writer_task_terminal_state(
            active_result,
            WriterTaskRequestState::TransactionRolledBack,
        );

        let queued_result = queued.await.expect("terminal drain must reply");
        assert_writer_task_terminal_state(queued_result, WriterTaskRequestState::NotStarted);

        let after = pool.writer_acquisition_snapshot();
        assert_eq!(
            after.writer_task_request_failures, 1,
            "only the request that actually reached the seam counts, not the ones \
                 failed by the queue-close drain"
        );
        assert_eq!(
            after.writer_task_side_effects_unknown, 0,
            "a clean rollback must not be counted as an unknown-side-effects outcome"
        );
    }

    // Case 2: a request whose rollback is itself refused (denied by an
    // installed authorizer) terminates `SideEffectsUnknown` and must move
    // both counters by exactly one.
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_failure_counters_unknown.db");
        let pool = file_pool(&path);
        let handle = spawn(&pool, 8).expect("writer task should spawn");

        let before = pool.writer_acquisition_snapshot();

        let active_result = handle
            .send(|conn| -> Result<(), StorageError> {
                conn.authorizer(Some(deny_rollback))
                    .map_err(|e| StorageError::Pool {
                        operation: "test_install_authorizer".into(),
                        message: e.to_string(),
                    })?;
                Err(StorageError::Internal(
                    "intentional operation failure before denied rollback".into(),
                ))
            })
            .await;
        assert_writer_task_terminal_state(
            active_result,
            WriterTaskRequestState::SideEffectsUnknown,
        );

        // The reply above races the counter increment: `send` observes
        // its reply before `run_writer_task` finishes its own await and
        // updates the pool counters. A trailing request against the same
        // (now-terminal) handle can only resolve once that update and the
        // subsequent queue-close drain have both happened, so awaiting it
        // orders the snapshot below strictly after the increment.
        let sentinel_result = handle.send(|_conn| Ok::<(), StorageError>(())).await;
        assert_writer_task_terminal_state(sentinel_result, WriterTaskRequestState::NotStarted);

        let after = pool.writer_acquisition_snapshot();
        assert_eq!(
            after.writer_task_request_failures - before.writer_task_request_failures,
            1
        );
        assert_eq!(
            after.writer_task_side_effects_unknown - before.writer_task_side_effects_unknown,
            1
        );
    }
}

#[path = "writer_task_admission_metrics_tests.rs"]
mod admission_metrics_tests;
