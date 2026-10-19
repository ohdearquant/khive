use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use khive_storage::tx_registry::{self, TxId, TxOrigin, TxOriginFilter};
use khive_storage::{SqlAccess as _, StorageError};
use tokio::sync::{oneshot, Semaphore};

use crate::pool::PoolConfig;

use super::{AtomicUnitOp, ConnectionPool, SqlBridge, SqlStatement, SqlValue};

const WATCHDOG: Duration = Duration::from_secs(5);

tokio::task_local! {
    static TEST_CONTEXT: Arc<TestContext>;
}

pub(super) fn current_test_context() -> Option<Arc<TestContext>> {
    TEST_CONTEXT.try_with(Arc::clone).ok()
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

fn transaction_view(pool: &ConnectionPool) -> TxOriginFilter {
    match pool.origin() {
        TxOrigin::Database(identity) => TxOriginFilter::Secondary(identity),
        other => panic!("expected file-backed origin, got {other:?}"),
    }
}

fn transaction_id(view: &TxOriginFilter) -> Option<TxId> {
    tx_registry::oldest_for(view)
        .filter(|span| span.label.as_deref() == Some("atomic_unit"))
        .map(|span| span.id)
}

#[derive(Debug)]
struct Ownership {
    transaction: Option<TxId>,
    available_writer_permits: usize,
    lock: LockState,
}

fn ownership(pool: &ConnectionPool, lock: &Path) -> Ownership {
    Ownership {
        transaction: transaction_id(&transaction_view(pool)),
        available_writer_permits: pool.sql_bridge_writer_slots().available_permits(),
        lock: lock_state(lock),
    }
}

#[derive(Debug)]
struct WorkerEntered {
    in_transaction: bool,
    ownership: Ownership,
}

pub(super) struct TestContext {
    lock: PathBuf,
    entered: Mutex<Option<oneshot::Sender<WorkerEntered>>>,
    release: Mutex<mpsc::Receiver<()>>,
    closed: Mutex<Option<oneshot::Sender<Ownership>>>,
}

struct CloseWitness {
    lock: PathBuf,
    view: TxOriginFilter,
    slots: Arc<Semaphore>,
    closed: Option<oneshot::Sender<Ownership>>,
}

impl Drop for CloseWitness {
    fn drop(&mut self) {
        let observed = Ownership {
            transaction: transaction_id(&self.view),
            available_writer_permits: self.slots.available_permits(),
            lock: lock_state(&self.lock),
        };
        if let Some(closed) = self.closed.take() {
            let _ = closed.send(observed);
        }
    }
}

impl TestContext {
    pub(super) fn before_body(
        &self,
        pool: &ConnectionPool,
        conn: &rusqlite::Connection,
    ) -> Result<(), StorageError> {
        let witness = CloseWitness {
            lock: self.lock.clone(),
            view: transaction_view(pool),
            slots: pool.sql_bridge_writer_slots(),
            closed: self.closed.lock().unwrap().take(),
        };
        conn.create_scalar_function(
            "manual_atomic_owner_close_witness",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |_| {
                let _keep_until_native_close = &witness;
                Ok(0_i64)
            },
        )
        .map_err(|error| {
            StorageError::driver(khive_storage::StorageCapability::Sql, "test", error)
        })?;
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(WorkerEntered {
                in_transaction: !conn.is_autocommit(),
                ownership: ownership(pool, &self.lock),
            });
        }
        self.release
            .lock()
            .unwrap()
            .recv_timeout(WATCHDOG)
            .map_err(|error| StorageError::Internal(format!("manual atomic test gate: {error}")))
    }
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

fn pool(path: PathBuf, locks: &Path) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path),
        volume_lock_dir: Some(locks.to_path_buf()),
        write_queue_enabled: Some(false),
        write_routing_strict: false,
        ..PoolConfig::for_test()
    })
    .unwrap()
}

fn create_probe(pool: &ConnectionPool) {
    pool.writer()
        .unwrap()
        .execute_batch("CREATE TABLE manual_atomic_owner_probe (id INTEGER PRIMARY KEY)")
        .unwrap();
}

fn insert(id: i64) -> SqlStatement {
    SqlStatement::new(
        "INSERT INTO manual_atomic_owner_probe VALUES (?1)",
        vec![SqlValue::Integer(id)],
    )
    .labelled("manual_atomic_owner_probe")
}

fn insert_pair() -> AtomicUnitOp {
    Box::new(|writer| {
        Box::pin(async move {
            writer.execute(insert(1)).await?;
            writer.execute(insert(2)).await?;
            let count = writer
                .query_scalar(SqlStatement::new(
                    "SELECT COUNT(*) FROM manual_atomic_owner_probe",
                    vec![],
                ))
                .await?;
            Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
        })
    })
}

async fn count(bridge: &SqlBridge) -> i64 {
    let value = bridge
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement::new(
            "SELECT COUNT(*) FROM manual_atomic_owner_probe",
            vec![],
        ))
        .await
        .unwrap();
    match value {
        Some(SqlValue::Integer(value)) => value,
        other => panic!("expected row count, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_atomic_owner_abort_retains_lease_permit_and_registry_until_settlement() {
    for abort_observer in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let locks = dir.path().join("volume-locks");
        let mut primary = pool(dir.path().join("primary.db"), &locks);
        let peer = pool(dir.path().join("peer.db"), &locks);
        create_probe(&primary);
        create_probe(&peer);
        primary.set_test_write_admission(100, |_| Ok(1_000));
        let primary = Arc::new(primary);
        let identity =
            crate::disk_guard::VolumeIdentity::resolve(primary.canonical_path().unwrap()).unwrap();
        assert!(
            identity
                == crate::disk_guard::VolumeIdentity::resolve(peer.canonical_path().unwrap())
                    .unwrap()
        );
        let lock = locks.join(identity.lock_filename());
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let (closed_tx, mut closed_rx) = oneshot::channel();
        let context = Arc::new(TestContext {
            lock: lock.clone(),
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(release_rx),
            closed: Mutex::new(Some(closed_tx)),
        });
        let bridge = Arc::new(SqlBridge::new(Arc::clone(&primary), true));
        let task_bridge = Arc::clone(&bridge);
        let mut call = Some(tokio::spawn(TEST_CONTEXT.scope(context, async move {
            task_bridge.atomic_unit(insert_pair()).await
        })));
        let entered = tokio::time::timeout(WATCHDOG, entered_rx)
            .await
            .expect("manual worker never reached the post-BEGIN gate")
            .unwrap();
        let before_abort = ownership(&primary, &lock);
        let cancelled = if abort_observer {
            let call = call.take().unwrap();
            call.abort();
            matches!(call.await, Err(error) if error.is_cancelled())
        } else {
            false
        };
        let after_abort = ownership(&primary, &lock);
        let premature_close = closed_rx.try_recv().ok();

        // Open the gate before asserting, including for deliberately broken owners.
        release.release();
        let completed = match call {
            Some(call) => Some(tokio::time::timeout(WATCHDOG, call).await.unwrap().unwrap()),
            None => None,
        };
        let (closed_early, closed) = match premature_close {
            Some(closed) => (true, closed),
            None => (
                false,
                tokio::time::timeout(WATCHDOG, closed_rx)
                    .await
                    .expect("native manual owner did not close")
                    .unwrap(),
            ),
        };
        let recovered_slot =
            tokio::time::timeout(WATCHDOG, primary.sql_bridge_writer_slots().acquire_owned())
                .await
                .expect("native manual owner did not return its writer permit")
                .unwrap();
        drop(recovered_slot);
        let after_settlement = ownership(&primary, &lock);
        peer.writer()
            .unwrap()
            .execute_batch("INSERT INTO manual_atomic_owner_probe VALUES (1)")
            .expect("same-volume peer must proceed after native settlement");

        assert!(entered.in_transaction);
        let transaction = entered
            .ownership
            .transaction
            .expect("BEGIN was not registered");
        for observed in [&entered.ownership, &before_abort, &after_abort, &closed] {
            assert_eq!(observed.transaction, Some(transaction), "{observed:?}");
            assert_eq!(observed.available_writer_permits, 0, "{observed:?}");
            assert_eq!(observed.lock, LockState::Held, "{observed:?}");
        }
        assert!(
            !closed_early,
            "native owner closed while its worker was gated"
        );
        assert_eq!(after_settlement.transaction, None);
        assert_eq!(after_settlement.available_writer_permits, 1);
        assert_eq!(after_settlement.lock, LockState::Available);
        if abort_observer {
            assert!(cancelled, "outer atomic observer was not actually aborted");
        } else {
            assert!(completed.unwrap().is_ok());
        }
        assert_eq!(
            count(&bridge).await,
            2,
            "the whole admitted unit must commit"
        );
    }
}

#[tokio::test]
async fn manual_atomic_samples_after_begin_once_and_finalizes_after_floor_drops() {
    for fail_body in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manual-timing.db");
        let mut pool = pool(path.clone(), &dir.path().join("volume-locks"));
        create_probe(&pool);
        let available = Arc::new(AtomicU64::new(1_000));
        let probes = Arc::new(AtomicUsize::new(0));
        let after_begin = Arc::new(AtomicUsize::new(0));
        let (sampled, observed, locked) = (
            Arc::clone(&available),
            Arc::clone(&probes),
            Arc::clone(&after_begin),
        );
        pool.set_test_write_admission(100, move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            let contender = rusqlite::Connection::open(&path).unwrap();
            contender.busy_timeout(Duration::ZERO).unwrap();
            match contender.execute_batch("BEGIN IMMEDIATE") {
                Err(error) if crate::timeout_sink::is_busy_or_locked(&error) => {
                    locked.fetch_add(1, Ordering::SeqCst);
                }
                Ok(()) => contender.execute_batch("ROLLBACK").unwrap(),
                Err(error) => panic!("unexpected transaction-timing error: {error}"),
            }
            Ok(sampled.load(Ordering::SeqCst))
        });
        let pool = Arc::new(pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let result = bridge
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    writer.execute(insert(1)).await?;
                    available.store(0, Ordering::SeqCst);
                    if fail_body {
                        return Err(StorageError::Internal("manual body failure".into()));
                    }
                    writer.execute(insert(2)).await?;
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                })
            }))
            .await;
        if fail_body {
            assert!(matches!(
                result,
                Err(StorageError::Internal(message)) if message == "manual body failure"
            ));
        } else {
            assert!(result.is_ok());
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert_eq!(after_begin.load(Ordering::SeqCst), 1);
        assert_eq!(count(&bridge).await, if fail_body { 0 } else { 2 });
        assert_eq!(pool.sql_bridge_writer_slots().available_permits(), 1);
        assert_eq!(transaction_id(&transaction_view(&pool)), None);
    }
}

#[tokio::test]
async fn manual_atomic_pending_and_panicking_callbacks_settle_before_owner_release() {
    for panic_body in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(pool(
            dir.path().join("manual-unwind.db"),
            &dir.path().join("volume-locks"),
        ));
        create_probe(&pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let error = tokio::time::timeout(
            WATCHDOG,
            bridge.atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    writer.execute(insert(1)).await?;
                    assert!(!panic_body, "manual atomic callback panic");
                    std::future::pending::<()>().await;
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                })
            })),
        )
        .await
        .expect("manual callback did not reach its settlement boundary")
        .unwrap_err();
        if panic_body {
            assert!(matches!(
                error,
                StorageError::WriterTaskTerminated {
                    request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                    ..
                }
            ));
        } else {
            assert!(matches!(
                error,
                StorageError::Internal(message) if message.contains("future suspended")
            ));
        }
        assert_eq!(count(&bridge).await, 0);
        assert_eq!(pool.sql_bridge_writer_slots().available_permits(), 1);
        assert_eq!(transaction_id(&transaction_view(&pool)), None);
        bridge.atomic_unit(insert_pair()).await.unwrap();
        assert_eq!(count(&bridge).await, 2);
    }
}
