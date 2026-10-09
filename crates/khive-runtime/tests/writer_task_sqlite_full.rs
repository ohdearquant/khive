//! Real admitted writer failure through the caller-visible runtime projection.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use khive_db::{
    writer_task, ConnectionPool, DiskGuardConfigSource, EffectiveDiskGuardConfig, PoolConfig,
    WalCeilingPolicy, WalCeilingSource,
};
use khive_runtime::{runtime_error_value, DomainDisposition, RuntimeError};
use khive_storage::{StorageCapability, StorageError, WriterTaskRequestState};

fn storage_error(error: rusqlite::Error) -> StorageError {
    StorageError::driver(StorageCapability::Sql, "native_full_projection", error)
}

#[test]
fn automatic_rollback_full_reaches_runtime_projection() {
    const ROOT: &str = "KHIVE_RUNTIME_FULL_FIXTURE_ROOT";
    let dir = tempfile::tempdir().expect("private fixture root");
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_RUNTIME_FULL_FIXTURE_CHILD",
        false,
        |command| {
            command.env(ROOT, dir.path());
            command.env("KHIVE_TEST_HARNESS", "1");
            command.env("KHIVE_WRITER_TIMEOUT_SINK_DIR", dir.path().join("sink"));
            command.env("KHIVE_WRITER_TIMEOUT_SINK_HEARTBEAT_MS", "100");
            command.env("KHIVE_WRITER_TIMEOUT_SINK_WRITE_DELAY_MS", "0");
            command.env("KHIVE_SLOW_WRITE_THRESHOLD_MS", "0");
            command.env_remove("KHIVE_WRITER_TIMEOUT_SINK_STARTUP_BARRIER_DIR");
        },
    ) {
        return;
    }

    let root = PathBuf::from(std::env::var_os(ROOT).expect("configured child root"));
    std::fs::create_dir_all(root.join("sink")).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let pool = ConnectionPool::new(PoolConfig {
                path: Some(root.join("bounded.db")),
                code_map_vfs: None,
                #[cfg(any(unix, windows))]
                expected_file_identity: None,
                max_readers: 1,
                reader_max_age: Duration::from_secs(300),
                reader_max_ops: 5000,
                wal_mode: true,
                busy_timeout: Duration::from_secs(1),
                checkout_timeout: Duration::from_secs(1),
                reader_checkout_warn_after: Duration::from_secs(10),
                journal_size_limit_bytes: 64 * 1024 * 1024,
                read_only: false,
                wal_ceiling: WalCeilingPolicy {
                    bytes: 0,
                    source: WalCeilingSource::BackendField,
                },
                write_queue_enabled: Some(false),
                write_queue_capacity: 8,
                write_routing_strict: false,
                write_admission_deadline_ms: 2000,
                disk_guard_config: Some(EffectiveDiskGuardConfig {
                    reserve_bytes: 0,
                    guard_deadline_ms: 2000,
                    reserve_source: DiskGuardConfigSource::Backend,
                    deadline_source: DiskGuardConfigSource::Backend,
                    legacy_environment_present: false,
                }),
                volume_lock_dir: Some(root.join("volume-locks")),
                read_tx_max_age: Duration::from_secs(120),
            })
            .expect("private pool with explicit policies");
            let handle = writer_task::spawn(&pool, 8).expect("actual writer task");
            let one = tokio::time::timeout(
                Duration::from_secs(10),
                handle.send(|conn| {
                    conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                        .map_err(storage_error)
                }),
            )
            .await
            .expect("positive request completes")
            .expect("positive request succeeds");
            assert_eq!(one, 1);

            let (pages, cap) = tokio::time::timeout(
                Duration::from_secs(10),
                handle.send_top_level(|conn| {
                    conn.execute_batch("CREATE TABLE payload(bytes BLOB NOT NULL)")
                        .map_err(storage_error)?;
                    let pages: i64 = conn
                        .query_row("PRAGMA page_count", [], |row| row.get(0))
                        .map_err(storage_error)?;
                    let cap: i64 = conn
                        .query_row(
                            &format!("PRAGMA max_page_count = {}", pages + 1),
                            [],
                            |row| row.get(0),
                        )
                        .map_err(storage_error)?;
                    Ok((pages, cap))
                }),
            )
            .await
            .expect("fixture completes")
            .expect("page limit set on writer connection");
            assert_eq!(cap, pages + 1);

            let (observed_tx, observed_rx) = mpsc::channel();
            let failed: Result<(), StorageError> = tokio::time::timeout(
                Duration::from_secs(10),
                handle.send(move |conn| {
                    let was_in_transaction = !conn.is_autocommit();
                    match conn.execute("INSERT INTO payload VALUES (zeroblob(1048576))", []) {
                        Ok(_) => {
                            observed_tx
                                .send((None, was_in_transaction, conn.is_autocommit()))
                                .unwrap();
                            Ok(())
                        }
                        Err(error) => {
                            let codes = match &error {
                                rusqlite::Error::SqliteFailure(code, _)
                                    if code.code == rusqlite::ErrorCode::DiskFull =>
                                {
                                    Some((code.extended_code & 0xff, code.extended_code))
                                }
                                _ => None,
                            };
                            observed_tx
                                .send((codes, was_in_transaction, conn.is_autocommit()))
                                .unwrap();
                            Err(storage_error(error))
                        }
                    }
                }),
            )
            .await
            .expect("bounded failing request completes");
            let (raw_codes, was_in_transaction, auto_rolled_back) =
                observed_rx.try_recv().expect("actual body observation");
            let raw_codes =
                raw_codes.expect("actual native DiskFull, not a synthetic or setup error");
            assert_eq!(raw_codes.0, rusqlite::ffi::SQLITE_FULL);
            assert!(was_in_transaction, "writer must start the transaction");
            assert!(auto_rolled_back, "SQLite must automatically end it");
            let error = failed.expect_err("bounded insert must fail");
            assert!(matches!(&error, StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
            sqlite_full_codes: Some(codes),
        } if *codes == raw_codes));
            assert_eq!(error.capability(), None);
            assert!(!error.is_retryable());
            assert!(std::error::Error::source(&error).is_none());
            let value =
                runtime_error_value(RuntimeError::Storage(error), DomainDisposition::Unknown);
            assert_eq!(value["stage"], "sqlite_disk_full");
            assert_eq!(value["code"], "sqlite_disk_full");
            assert_eq!(value["capability"], "sql");
            assert_eq!(value["sqlite_primary_code"], raw_codes.0);
            assert_eq!(value["sqlite_extended_code"], raw_codes.1);
            assert_eq!(value["request_state"], "side_effects_unknown");
            assert_eq!(value["task_terminated"], true);
            assert_eq!(value["retryable"], false);
            assert_eq!(value["domain_disposition"], "unknown");
            assert_ne!(value["stage"], "sqlite_capacity_refused");

            let later = tokio::time::timeout(Duration::from_secs(10), handle.send(|_| Ok(())))
                .await
                .expect("retired seam replies");
            assert!(matches!(
                later,
                Err(StorageError::WriterTaskTerminated {
                    request_state: WriterTaskRequestState::NotStarted,
                    sqlite_full_codes: None,
                })
            ));
            drop(handle);
            let join = pool
                .take_writer_task_join()
                .expect("actual spawned writer join");
            tokio::time::timeout(Duration::from_secs(10), join)
                .await
                .expect("retired writer exits")
                .expect("writer join");
        });
}
