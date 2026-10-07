use super::*;
use crate::disk_guard::VolumeIdentity;
use crate::pool::PoolConfig;
use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn lifetime_writer_refuses_changed_volume_after_begin_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queued-volume.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        // A 100 ms lease deadline is only meaningful in the test's own lock
        // namespace; in the shared one it queues behind other tests' writers.
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(
            crate::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
        ),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let admission = pool.write_admission();
    let handle = spawn(&pool, 8).unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let arm = Arc::clone(&changed);
    let inside = Arc::clone(&admission);
    let wrong = VolumeIdentity::resolve(&path)
        .unwrap()
        .different_volume_for_test();
    handle
        .send(move |conn| {
            conn.execute_batch("CREATE TABLE guarded (id INTEGER PRIMARY KEY)")
                .unwrap();
            conn.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
                if matches!(
                    ctx.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Begin
                    }
                ) && !arm.swap(true, Ordering::SeqCst)
                {
                    inside.set_test_current_volume(Some(wrong.clone()));
                }
                Authorization::Allow
            }))
            .unwrap();
            Ok::<_, StorageError>(())
        })
        .await
        .unwrap();
    assert!(
        !changed.load(Ordering::SeqCst),
        "identity changes only on the next BEGIN"
    );
    let ran = Arc::new(AtomicBool::new(false));
    let in_body = Arc::clone(&ran);
    let result = handle
        .send(move |conn| {
            in_body.store(true, Ordering::SeqCst);
            conn.execute("INSERT INTO guarded VALUES (1)", []).unwrap();
            Ok::<_, StorageError>(())
        })
        .await;
    assert!(
        changed.load(Ordering::SeqCst),
        "actual queued BEGIN must execute"
    );
    assert!(
        matches!(
            result,
            Err(StorageError::CapacityUnavailable {
                phase: khive_storage::CapacityUnavailablePhase::Identity,
                ..
            })
        ),
        "LIFETIME_WRITER_VOLUME_BINDING: the post-BEGIN mismatch must refuse the actual request"
    );
    assert!(!ran.load(Ordering::SeqCst));
    admission.set_test_current_volume(None);
    handle
        .send(|conn| {
            assert!(!conn.is_autocommit(), "recovery request has its own BEGIN");
            conn.execute("INSERT INTO guarded VALUES (2)", []).unwrap();
            Ok::<_, StorageError>(())
        })
        .await
        .expect("identity refusal must roll back without terminating the writer");
    let reader = pool.reader().unwrap();
    let rows: Vec<i64> = reader
        .conn()
        .prepare("SELECT id FROM guarded ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, [2], "refused callback must leave no row");
}
