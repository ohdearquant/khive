//! Public cached-writer admission and settlement witnesses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use khive_storage::{SqlAccess as _, SqlWriter, StorageError};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use tokio::sync::{oneshot, Semaphore};

use crate::disk_guard::VolumeIdentity;
use crate::pool::PoolConfig;

use super::{ConnectionPool, PageRequest, SqlBridge, SqlRow, SqlStatement};

const WATCHDOG: Duration = Duration::from_secs(5);

fn statement(sql: &str) -> SqlStatement {
    SqlStatement::new(sql, vec![])
}

#[derive(Clone)]
struct Probe {
    available: Arc<AtomicU64>,
    calls: Arc<AtomicUsize>,
    threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}

fn pool(root: &Path, queued: bool) -> (Arc<ConnectionPool>, Probe) {
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(root.join("writer-query.db")),
        volume_lock_dir: Some(root.join("volume-locks")),
        write_queue_enabled: Some(queued),
        write_routing_strict: queued,
        max_readers: 2,
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.standalone_transaction(|conn| {
        conn.execute_batch(
            "CREATE TABLE writable_query_rows (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
             INSERT INTO writable_query_rows VALUES (1, 10);
             CREATE TABLE writable_query_effects (operation TEXT NOT NULL);
             CREATE TRIGGER writable_query_insert AFTER INSERT ON writable_query_rows
             BEGIN INSERT INTO writable_query_effects VALUES ('insert'); END;
             CREATE TRIGGER writable_query_update AFTER UPDATE ON writable_query_rows
             BEGIN INSERT INTO writable_query_effects VALUES ('update'); END;",
        )?;
        Ok(())
    })
    .unwrap();
    let probe = Probe {
        available: Arc::new(AtomicU64::new(1_000)),
        calls: Arc::new(AtomicUsize::new(0)),
        threads: Arc::new(Mutex::new(Vec::new())),
    };
    let observed = probe.clone();
    pool.set_test_write_admission(100, move |_| {
        observed.calls.fetch_add(1, Ordering::SeqCst);
        observed
            .threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        Ok(observed.available.load(Ordering::SeqCst))
    });
    (Arc::new(pool), probe)
}

async fn snapshot(bridge: &SqlBridge) -> (i64, i64, i64) {
    let row = bridge
        .reader()
        .await
        .unwrap()
        .query_row(statement(
            "SELECT (SELECT COUNT(*) FROM writable_query_rows) AS rows,
                    (SELECT SUM(value) FROM writable_query_rows) AS total,
                    (SELECT COUNT(*) FROM writable_query_effects) AS effects",
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.i64("rows").unwrap(),
        row.i64("total").unwrap(),
        row.i64("effects").unwrap(),
    )
}

#[derive(Clone, Copy, Debug)]
enum QueryRoute {
    Row,
    All,
    Page,
}

async fn query(
    writer: &mut dyn SqlWriter,
    route: QueryRoute,
    sql: SqlStatement,
) -> Result<Vec<SqlRow>, StorageError> {
    match route {
        QueryRoute::Row => writer
            .query_row(sql)
            .await
            .map(|row| row.into_iter().collect()),
        QueryRoute::All => writer.query_all(sql).await,
        QueryRoute::Page => {
            writer
                .query_page(
                    sql,
                    PageRequest {
                        offset: 0,
                        limit: 10,
                    },
                )
                .await
        }
    }
}

fn returning_values(rows: &[SqlRow]) -> Vec<(i64, i64)> {
    rows.iter()
        .map(|row| (row.i64("id").unwrap(), row.i64("value").unwrap()))
        .collect()
}

#[tokio::test]
async fn cached_writable_queries_sample_at_execution_and_recover_returning_rows() {
    for route in [QueryRoute::Row, QueryRoute::All, QueryRoute::Page] {
        for update in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (pool, probe) = pool(root.path(), false);
            let bridge = SqlBridge::new(Arc::clone(&pool), true);
            let mut writer = bridge.writer().await.unwrap();
            assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
            let request_thread = std::thread::current().id();
            let sql = if update {
                "UPDATE writable_query_rows SET value = 20 WHERE id = 1 RETURNING id, value"
            } else {
                "INSERT INTO writable_query_rows VALUES (2, 20) RETURNING id, value"
            };

            // The handle was acquired above the floor; admission must use the
            // changed execution-time sample, before either trigger/body runs.
            probe.available.store(0, Ordering::SeqCst);
            let error = query(&mut *writer, route, statement(sql))
                .await
                .expect_err("a cached writable query must sample at execution");
            assert!(
                matches!(
                    error.without_sqlite_write_stage(),
                    StorageError::CapacityFloor { .. }
                ),
                "{route:?} update={update}: {error:?}"
            );
            assert_eq!(snapshot(&bridge).await, (1, 10, 0));
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

            probe.available.store(1_000, Ordering::SeqCst);
            let rows = query(&mut *writer, route, statement(sql))
                .await
                .expect("capacity recovery must leave the same cached handle usable");
            assert_eq!(returning_values(&rows), [(if update { 1 } else { 2 }, 20)]);
            assert_eq!(
                snapshot(&bridge).await,
                if update { (1, 20, 1) } else { (2, 30, 1) }
            );
            assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
            assert!(probe
                .threads
                .lock()
                .unwrap()
                .iter()
                .all(|thread| *thread != request_thread));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockState {
    Held,
    Available,
    Unavailable,
}

fn lock_state(path: &Path) -> LockState {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return LockState::Unavailable;
    };
    match fs4::FileExt::try_lock(&file) {
        Err(fs4::TryLockError::WouldBlock) => LockState::Held,
        Ok(()) => LockState::Available,
        Err(_) => LockState::Unavailable,
    }
}

#[derive(Debug)]
struct Ownership {
    lock: LockState,
    available_permits: usize,
}

fn ownership(lock: &Path, slots: &Semaphore) -> Ownership {
    Ownership {
        lock: lock_state(lock),
        available_permits: slots.available_permits(),
    }
}

struct NativeGate {
    lock: PathBuf,
    slots: Arc<Semaphore>,
    entered: Mutex<Option<oneshot::Sender<Ownership>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

struct ConnectionHooks {
    lock: PathBuf,
    slots: Arc<Semaphore>,
    gate: Option<Arc<NativeGate>>,
    deny_rollback: bool,
    terminal: bool,
    closed: Mutex<Option<oneshot::Sender<Ownership>>>,
}

static CONNECTION_HOOKS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ConnectionHooks>>>> = OnceLock::new();

struct HookRegistration(PathBuf);

impl Drop for HookRegistration {
    fn drop(&mut self) {
        if let Some(hooks) = CONNECTION_HOOKS.get() {
            if let Ok(mut hooks) = hooks.lock() {
                hooks.remove(&self.0);
            }
        }
    }
}

fn register(pool: &ConnectionPool, hooks: Arc<ConnectionHooks>) -> HookRegistration {
    let path = pool.canonical_path().unwrap().to_path_buf();
    let previous = CONNECTION_HOOKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .insert(path.clone(), hooks);
    assert!(
        previous.is_none(),
        "a fixture must own its unique database hook"
    );
    HookRegistration(path)
}

struct CloseWitness {
    lock: PathBuf,
    slots: Arc<Semaphore>,
    closed: Option<oneshot::Sender<Ownership>>,
}

impl Drop for CloseWitness {
    fn drop(&mut self) {
        if let Some(closed) = self.closed.take() {
            let _ = closed.send(ownership(&self.lock, &self.slots));
        }
    }
}

/// Called only by the cfg(test) complete standalone-writer factory seam.
/// Hooks are keyed by a unique fixture path and removed by its registration.
pub(super) fn configure_test_connection(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
) -> Result<(), StorageError> {
    let Some(path) = pool.canonical_path() else {
        return Ok(());
    };
    let hooks = CONNECTION_HOOKS
        .get()
        .and_then(|hooks| hooks.lock().unwrap().get(path).cloned());
    let Some(hooks) = hooks else {
        return Ok(());
    };
    let map_error = |error| {
        StorageError::driver(
            khive_storage::StorageCapability::Sql,
            "writer query fixture",
            error,
        )
    };
    let witness = CloseWitness {
        lock: hooks.lock.clone(),
        slots: Arc::clone(&hooks.slots),
        closed: hooks.closed.lock().unwrap().take(),
    };
    conn.create_scalar_function(
        "writer_query_close_witness",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |_| {
            let _keep_until_native_close = &witness;
            Ok(0_i64)
        },
    )
    .map_err(map_error)?;
    if let Some(gate) = hooks.gate.clone() {
        conn.create_scalar_function(
            "writer_query_native_gate",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |context| {
                let value = context.get::<i64>(0)?;
                if let Some(entered) = gate.entered.lock().unwrap().take() {
                    let _ = entered.send(ownership(&gate.lock, &gate.slots));
                }
                gate.release
                    .lock()
                    .unwrap()
                    .recv_timeout(WATCHDOG)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
                Ok(value)
            },
        )
        .map_err(map_error)?;
    }
    if hooks.terminal {
        // Only isolated Unix children request this native close-BUSY fault.
        // The forgotten statement and worker-local denial must not escape into
        // other tests in the parent's reusable blocking pool.
        std::mem::forget(conn.prepare("SELECT 1").map_err(map_error)?);
        eprintln!("WRITER_QUERY_UNSETTLED_STATEMENT_LEAK");
    }
    if hooks.deny_rollback {
        let terminal = hooks.terminal;
        conn.authorizer(Some(move |context: AuthContext<'_>| match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => {
                #[cfg(unix)]
                if terminal {
                    crate::connection_settlement::force_rollback_denial_for_test();
                }
                #[cfg(not(unix))]
                let _ = terminal;
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }))
        .map_err(map_error)?;
    }
    Ok(())
}

fn hooks(
    pool: &ConnectionPool,
    deny_rollback: bool,
    terminal: bool,
) -> (Arc<ConnectionHooks>, oneshot::Receiver<Ownership>) {
    let identity = VolumeIdentity::resolve(pool.canonical_path().unwrap()).unwrap();
    let lock = pool
        .config()
        .volume_lock_dir
        .as_ref()
        .unwrap()
        .join(identity.lock_filename());
    let (closed, closed_rx) = oneshot::channel();
    (
        Arc::new(ConnectionHooks {
            lock,
            slots: pool.sql_bridge_writer_slots(),
            gate: None,
            deny_rollback,
            terminal,
            closed: Mutex::new(Some(closed)),
        }),
        closed_rx,
    )
}

struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

impl ReleaseOnDrop {
    fn release(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_writable_query_keeps_lease_during_native_work_and_observer_abort() {
    for abort_observer in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (pool, probe) = pool(root.path(), false);
        let (mut hooks, closed) = hooks(&pool, false, false);
        let (entered, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let hook = Arc::get_mut(&mut hooks).unwrap();
        hook.gate = Some(Arc::new(NativeGate {
            lock: hook.lock.clone(),
            slots: Arc::clone(&hook.slots),
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(release_rx),
        }));
        let _registration = register(&pool, Arc::clone(&hooks));
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        let mut call = Some(tokio::spawn(crate::scope_request_read_cancellation(
            cancellation,
            async move {
                let result = writer
                    .query_all(statement(
                        "INSERT INTO writable_query_rows
                         VALUES (2, writer_query_native_gate(20)) RETURNING id, value",
                    ))
                    .await;
                drop(writer);
                result
            },
        )));
        let entered = tokio::time::timeout(WATCHDOG, entered_rx)
            .await
            .expect("the public writable query did not enter its native SQL gate")
            .unwrap();
        probe.available.store(0, Ordering::SeqCst);
        cancel.send(true).unwrap();
        let cancelled = if abort_observer {
            let call = call.take().unwrap();
            call.abort();
            matches!(call.await, Err(error) if error.is_cancelled())
        } else {
            false
        };
        let after_cancel = ownership(&hooks.lock, &hooks.slots);
        release.release();
        let result = if let Some(call) = call {
            Some(tokio::time::timeout(WATCHDOG, call).await.unwrap().unwrap())
        } else {
            None
        };
        let closed = tokio::time::timeout(WATCHDOG, closed)
            .await
            .expect("the completed or abandoned native owner did not close")
            .unwrap();
        let recovered =
            tokio::time::timeout(WATCHDOG, pool.sql_bridge_writer_slots().acquire_owned())
                .await
                .expect("the settled native call did not return the writer permit")
                .unwrap();
        assert_eq!(entered.lock, LockState::Held);
        assert_eq!(entered.available_permits, 0);
        assert_eq!(after_cancel.lock, LockState::Held);
        assert_eq!(after_cancel.available_permits, 0);
        assert_eq!(closed.available_permits, 0);
        assert_eq!(lock_state(&hooks.lock), LockState::Available);
        if abort_observer {
            assert!(cancelled, "the observer was not actually aborted");
        } else {
            assert_eq!(returning_values(&result.unwrap().unwrap()), [(2, 20)]);
        }
        assert_eq!(snapshot(&bridge).await, (2, 30, 1));
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        drop(recovered);
        drop(bridge.writer().await.unwrap());
    }
}

#[tokio::test]
async fn cached_writer_query_controls_cannot_outlive_the_call() {
    for route in [QueryRoute::Row, QueryRoute::All, QueryRoute::Page] {
        for sql in [
            "BEGIN IMMEDIATE",
            " /* caller */ ; BEGIN DEFERRED",
            "SAVEPOINT caller_span",
        ] {
            let root = tempfile::tempdir().unwrap();
            let (pool, _) = pool(root.path(), false);
            let bridge = SqlBridge::new(Arc::clone(&pool), true);
            let mut writer = bridge.writer().await.unwrap();
            let error = query(&mut *writer, route, statement(sql))
                .await
                .expect_err("an unowned transaction cannot survive a query call");
            assert!(
                matches!(
                    error.without_sqlite_write_stage(),
                    StorageError::InvalidInput { .. }
                ),
                "{route:?} {sql}: {error:?}"
            );
            assert_eq!(snapshot(&bridge).await, (1, 10, 0));
            // Both up-front refusal and execute-then-proven-rollback are legal.
            // A real fresh transaction on the same handle distinguishes them
            // from reporting an error while retaining the caller's BEGIN.
            writer.execute_batch(vec![]).await.unwrap();
            let rows = writer
                .query_all(statement(
                    "UPDATE writable_query_rows SET value = 20 WHERE id = 1 RETURNING id, value",
                ))
                .await
                .unwrap();
            assert_eq!(returning_values(&rows), [(1, 20)]);
            assert_eq!(snapshot(&bridge).await, (1, 20, 1));
        }
    }
}

#[tokio::test]
async fn pooled_writer_query_controls_settle_and_returning_remains_usable() {
    for route in [QueryRoute::Row, QueryRoute::All, QueryRoute::Page] {
        for sql in ["BEGIN IMMEDIATE", "SAVEPOINT caller_span"] {
            let pool = Arc::new(ConnectionPool::new(PoolConfig::for_test()).unwrap());
            let bridge = SqlBridge::new(pool, false);
            let mut writer = bridge.writer().await.unwrap();
            writer
                .execute(statement(
                    "CREATE TABLE pooled_query_rows (id INTEGER PRIMARY KEY, value INTEGER)",
                ))
                .await
                .unwrap();
            let error = query(&mut *writer, route, statement(sql))
                .await
                .expect_err("a pooled query cannot leave a transaction for a later call");
            assert!(
                matches!(
                    error.without_sqlite_write_stage(),
                    StorageError::InvalidInput { .. }
                ),
                "{route:?} {sql}: {error:?}"
            );
            writer.execute_batch(vec![]).await.unwrap();
            let rows = query(
                &mut *writer,
                route,
                statement("INSERT INTO pooled_query_rows VALUES (1, 20) RETURNING id, value"),
            )
            .await
            .unwrap();
            assert_eq!(returning_values(&rows), [(1, 20)]);
            let count = writer
                .query_row(statement("SELECT COUNT(*) AS rows FROM pooled_query_rows"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(count.i64("rows").unwrap(), 1);
        }
    }
}

fn assert_original_script_failure(error: &StorageError) {
    let StorageError::Driver { source, .. } = error.without_sqlite_write_stage() else {
        panic!("proven rollback must retain the script's native driver error: {error:?}")
    };
    let native = source
        .downcast_ref::<rusqlite::Error>()
        .expect("the original rusqlite source must remain available");
    assert!(
        matches!(native, rusqlite::Error::SqliteFailure(code, Some(message))
            if code.extended_code == rusqlite::ffi::SQLITE_ERROR
                && message.contains("writer_query_missing_table")),
        "{native:?}"
    );
}

fn unfinished_script(fail_body: bool) -> String {
    format!(
        "BEGIN IMMEDIATE; INSERT INTO writable_query_rows VALUES (2, 20); {}",
        if fail_body {
            "INSERT INTO writer_query_missing_table VALUES (1);"
        } else {
            ""
        }
    )
}

#[tokio::test]
async fn cached_failing_script_retains_native_error_after_proven_rollback() {
    let root = tempfile::tempdir().unwrap();
    let (pool, _) = pool(root.path(), false);
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let error = writer
        .execute_script(unfinished_script(true))
        .await
        .unwrap_err();
    assert_original_script_failure(&error);
    assert_eq!(snapshot(&bridge).await, (1, 10, 0));
    writer.execute_batch(vec![]).await.unwrap();
}

#[tokio::test]
async fn cached_script_owned_retirement_closes_under_lease_and_preserves_recovered_outcome() {
    for fail_body in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (pool, _) = pool(root.path(), false);
        let (hooks, closed) = hooks(&pool, true, false);
        let _registration = register(&pool, Arc::clone(&hooks));
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        let error = writer
            .execute_script(unfinished_script(fail_body))
            .await
            .unwrap_err();
        let closed = tokio::time::timeout(WATCHDOG, closed)
            .await
            .expect("the retired native connection must close before the call returns")
            .unwrap();
        assert_eq!(closed.lock, LockState::Held);
        assert_eq!(closed.available_permits, 0);
        assert_eq!(lock_state(&hooks.lock), LockState::Available);
        assert_eq!(pool.sql_bridge_writer_slots().available_permits(), 1);
        if fail_body {
            assert_original_script_failure(&error);
        } else {
            assert!(matches!(
                error.without_sqlite_write_stage(),
                StorageError::InvalidInput { .. }
            ));
        }
        assert_eq!(snapshot(&bridge).await, (1, 10, 0));
        assert!(writer.execute_batch(vec![]).await.is_err());
        drop(writer);
        // A first authorizer denial is recoverable once owned retirement
        // removes it. It must not impose the terminal pool-poison policy.
        pool.standalone_transaction(|_| Ok(())).unwrap();
    }
}

#[tokio::test]
async fn cached_reads_and_independent_deferred_read_spans_remain_exempt_below_floor() {
    for queued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (pool, probe) = pool(root.path(), queued);
        probe.available.store(0, Ordering::SeqCst);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        for route in [QueryRoute::Row, QueryRoute::All, QueryRoute::Page] {
            let rows = query(
                &mut *writer,
                route,
                statement("SELECT id, value FROM writable_query_rows"),
            )
            .await
            .expect("an admitted read shape on a writer remains floor-exempt");
            assert_eq!(returning_values(&rows), [(1, 10)]);
        }
        // This separate capability owns a read-only deferred span. A BEGIN
        // through the writable cached handle is covered by the control test.
        let mut reader = bridge.reader().await.unwrap();
        reader.query_all(statement("BEGIN DEFERRED")).await.unwrap();
        let rows = reader
            .query_all(statement("SELECT id, value FROM writable_query_rows"))
            .await
            .unwrap();
        assert_eq!(returning_values(&rows), [(1, 10)]);
        reader.query_all(statement("ROLLBACK")).await.unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }
}

#[cfg(unix)]
enum UnsettledCall {
    SuccessfulScript,
    FailingScript,
    QueryControl,
}

#[cfg(unix)]
async fn exercise_unsettled_child(root: &Path, call: UnsettledCall) {
    let (pool, _) = pool(root, false);
    let (hooks, mut closed) = hooks(&pool, true, true);
    let _registration = register(&pool, Arc::clone(&hooks));
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let result = match call {
        UnsettledCall::SuccessfulScript => writer.execute_script(unfinished_script(false)).await,
        UnsettledCall::FailingScript => writer.execute_script(unfinished_script(true)).await,
        UnsettledCall::QueryControl => writer
            .query_all(statement("BEGIN IMMEDIATE"))
            .await
            .map(|_| ()),
    };
    let error = result.unwrap_err();
    assert!(
        matches!(
            error.without_sqlite_write_stage(),
            StorageError::WriterTaskTerminated {
                request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                ..
            }
        ),
        "both retirement rollback and native close failed: {error:?}"
    );
    assert!(matches!(
        closed.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(lock_state(&hooks.lock), LockState::Available);
    assert_eq!(pool.sql_bridge_writer_slots().available_permits(), 1);
    assert!(matches!(
        pool.writer(),
        Err(crate::SqliteError::WriterPoisoned)
    ));
    assert!(matches!(
        pool.standalone_transaction::<()>(|_| panic!("a poisoned body must not run")),
        Err(crate::SqliteError::WriterPoisoned)
    ));
    assert!(writer.execute_batch(vec![]).await.is_err());
    eprintln!("WRITER_QUERY_UNSETTLED_CONTRACT_PASSED");
}

#[cfg(unix)]
#[tokio::test]
async fn cached_unsettled_script_reports_unknown_poisons_pool_and_releases_lease() {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    const ROOT: &str = "KHIVE_DB_WRITER_QUERY_UNSETTLED_ROOT";
    const CASE: &str = "KHIVE_DB_WRITER_QUERY_UNSETTLED_CASE";
    const TEST: &str = "sql_bridge::writer_query_admission_tests::cached_unsettled_script_reports_unknown_poisons_pool_and_releases_lease";
    if let Some(root) = std::env::var_os(ROOT) {
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            ["--exact", TEST, "--nocapture", "--test-threads=1"]
        );
        let call = match std::env::var(CASE).unwrap().as_str() {
            "successful_body" => UnsettledCall::SuccessfulScript,
            "failing_body" => UnsettledCall::FailingScript,
            "query_control" => UnsettledCall::QueryControl,
            other => panic!("unexpected child case {other}"),
        };
        exercise_unsettled_child(Path::new(&root), call).await;
        return;
    }

    for case in ["successful_body", "failing_body", "query_control"] {
        let root = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(ROOT, root.path())
            .env(CASE, case)
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdout_pipe = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        let status = match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                let kill_error = child.start_kill().err();
                let status = tokio::time::timeout(WATCHDOG, child.wait())
                    .await
                    .expect("timed-out fixture child must be reaped")
                    .unwrap();
                panic!(
                    "the isolated native-settlement child exceeded its watchdog: \
                     kill_error={kill_error:?}, status={status}"
                );
            }
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        stdout_pipe.read_to_end(&mut stdout).await.unwrap();
        stderr_pipe.read_to_end(&mut stderr).await.unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(status.success(), "{case}: {status}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        assert_eq!(
            stderr
                .matches("WRITER_QUERY_UNSETTLED_STATEMENT_LEAK")
                .count(),
            1
        );
        assert_eq!(
            stderr
                .matches("WRITER_QUERY_UNSETTLED_CONTRACT_PASSED")
                .count(),
            1
        );
        assert!(stderr.contains("owned SQLite settlement failed; outcome unknown"));
        assert!(stderr.contains("rollback_error=not authorized"));
        assert!(stderr.contains(
            "close_error=unable to close due to unfinalized statements or unfinished backups"
        ));

        // Assertions inside the child prove the live release+poison contract;
        // this independent native transaction proves recovery after its exit.
        let path = root.path().join("writer-query.db");
        let identity = VolumeIdentity::resolve(&path).unwrap();
        let _lease = identity
            .acquire(WATCHDOG, Some(&root.path().join("volume-locks")))
            .unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM writable_query_rows", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM writable_query_effects", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        conn.execute_batch("ROLLBACK").unwrap();
    }
}
