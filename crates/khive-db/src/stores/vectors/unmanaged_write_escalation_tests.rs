use super::*;
use crate::pool::PoolConfig;

struct CaptureReset;

impl Drop for CaptureReset {
    fn drop(&mut self) {
        crate::timeout_sink::capture_sqlite_full_for_test(None);
    }
}

#[tokio::test]
async fn vector_unmanaged_writer_escalates_sqlite_full() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("vectors.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        "full_test".into(),
        "full_test".into(),
        2,
        "local".into(),
    )
    .unwrap();
    let expected_db = crate::timeout_sink::db_label(&pool);
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::timeout_sink::capture_sqlite_full_for_test(Some(sender));
    let _capture_reset = CaptureReset;

    let error = store
        .with_writer_unmanaged("vec_full_test", |_conn| -> Result<(), rusqlite::Error> {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                Some("injected SQLite disk-full failure".into()),
            ))
        })
        .await
        .expect_err("legacy vector write must preserve the SQLite failure");
    assert!(matches!(error, StorageError::Driver { .. }));
    assert!(
        receiver.try_iter().any(|db| db == expected_db),
        "the unmanaged vector route must emit sqlite_full for its database"
    );

    let _ = store
        .with_writer_unmanaged("vec_busy_test", |_conn| -> Result<(), rusqlite::Error> {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ))
        })
        .await;
    assert!(
        receiver.try_iter().all(|db| db != expected_db),
        "busy is not sqlite_full"
    );
}
