use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};

fn pool(path: &Path, locks: &Path) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path.to_path_buf()),
        volume_lock_dir: Some(locks.to_path_buf()),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            guard_deadline_ms: 100,
            ..Default::default()
        }),
        ..PoolConfig::for_test()
    })
    .unwrap()
}

fn count(pool: &ConnectionPool) -> i64 {
    pool.reader()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM pooled_owner_probe", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn delayed_public_writer_checks_capacity_at_autocommit_execution() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = pool(&dir.path().join("delayed.db"), &dir.path().join("locks"));
    pool.writer()
        .unwrap()
        .autocommit(|conn| {
            conn.execute_batch("CREATE TABLE pooled_owner_probe(value INTEGER)")?;
            Ok(())
        })
        .unwrap();
    let available = Arc::new(AtomicU64::new(101));
    let samples = Arc::new(AtomicUsize::new(0));
    let sampled = Arc::clone(&available);
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(sampled.load(Ordering::SeqCst))
    });
    let writer = pool.writer().expect("checkout while capacity is healthy");
    assert_eq!(samples.load(Ordering::SeqCst), 0);
    available.store(100, Ordering::SeqCst);
    let body_ran = AtomicBool::new(false);
    let result = writer.autocommit(|conn| {
        body_ran.store(true, Ordering::SeqCst);
        conn.execute_batch("INSERT INTO pooled_owner_probe VALUES(1)")?;
        Ok(())
    });
    assert!(matches!(result, Err(SqliteError::CapacityFloor { .. })));
    assert!(!body_ran.load(Ordering::SeqCst));
    assert_eq!(count(&pool), 0);
    available.store(101, Ordering::SeqCst);
    pool.writer()
        .unwrap()
        .autocommit(|conn| {
            conn.execute_batch("INSERT INTO pooled_owner_probe VALUES(2)")?;
            Ok(())
        })
        .expect("capacity refusal returned the owner and its lease");
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    assert_eq!(count(&pool), 1);
}

#[test]
fn public_pooled_transaction_samples_after_begin_and_finalizes_below_floor() {
    for fail_body in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transaction.db");
        let mut pool = pool(&path, &dir.path().join("locks"));
        pool.writer()
            .unwrap()
            .autocommit(|conn| {
                conn.execute_batch("CREATE TABLE pooled_owner_probe(value INTEGER)")?;
                Ok(())
            })
            .unwrap();
        let available = Arc::new(AtomicU64::new(101));
        let samples = Arc::new(AtomicUsize::new(0));
        let sampled = Arc::clone(&available);
        let observed = Arc::clone(&samples);
        pool.set_test_write_admission(100, move |_| {
            let contender = Connection::open(&path).unwrap();
            contender.busy_timeout(Duration::ZERO).unwrap();
            let error = contender.execute_batch("BEGIN IMMEDIATE").unwrap_err();
            assert!(crate::timeout_sink::is_busy_or_locked(&error));
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(sampled.load(Ordering::SeqCst))
        });
        let writer = pool.try_writer().unwrap();
        assert_eq!(samples.load(Ordering::SeqCst), 0);
        let result = writer.transaction(|conn| {
            conn.execute_batch("INSERT INTO pooled_owner_probe VALUES(1)")?;
            available.store(0, Ordering::SeqCst);
            if fail_body {
                return Err(SqliteError::InvalidData("body failure".into()));
            }
            conn.execute_batch("INSERT INTO pooled_owner_probe VALUES(2)")?;
            Ok(())
        });
        if fail_body {
            assert!(
                matches!(result, Err(SqliteError::InvalidData(message)) if message == "body failure")
            );
        } else {
            result.unwrap();
        }
        assert_eq!(samples.load(Ordering::SeqCst), 1);
        assert_eq!(count(&pool), if fail_body { 0 } else { 2 });
    }
}

#[test]
fn pooled_script_refuses_unfinished_transactions_and_recovers_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool(&dir.path().join("script.db"), &dir.path().join("locks"));
    let result = pool.writer().unwrap().autocommit(|conn| {
        conn.execute_batch(
            "CREATE TABLE pooled_owner_probe(value INTEGER); \
             BEGIN IMMEDIATE; INSERT INTO pooled_owner_probe VALUES(1)",
        )?;
        Ok(())
    });
    assert!(matches!(result, Err(SqliteError::InvalidData(_))));
    assert_eq!(count(&pool), 0);
    pool.writer()
        .unwrap()
        .autocommit(|conn| {
            conn.execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO pooled_owner_probe VALUES(2); COMMIT",
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(count(&pool), 1);
}

#[test]
fn public_pooled_owners_preserve_native_full_and_existing_settlement_policy() {
    for transactional in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(&dir.path().join("full.db"), &dir.path().join("locks"));
        pool.writer()
            .unwrap()
            .autocommit(|conn| {
                conn.execute_batch("CREATE TABLE admission_payload(value BLOB)")?;
                let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
                conn.pragma_update(None, "max_page_count", pages + 1)?;
                Ok(())
            })
            .unwrap();
        let mut observed = None;
        let body = |conn: &Connection| -> Result<(), SqliteError> {
            let result = conn.execute(
                "INSERT INTO admission_payload VALUES (zeroblob(2097152))",
                [],
            );
            observed = Some((
                result
                    .as_ref()
                    .err()
                    .and_then(rusqlite::Error::sqlite_error_code),
                conn.is_autocommit(),
            ));
            result?;
            Ok(())
        };
        let writer = pool.writer().unwrap();
        let error = if transactional {
            writer.transaction(body)
        } else {
            writer.autocommit(body)
        }
        .expect_err("the per-connection page limit must produce native FULL");
        assert_eq!(observed, Some((Some(rusqlite::ErrorCode::DiskFull), true)));
        assert_eq!(
            error.write_failure(),
            Some(khive_storage::error::SqliteWriteFailure {
                stage: khive_storage::error::SqliteWriteStage::Statement,
                primary_code: rusqlite::ffi::SQLITE_FULL,
                extended_code: rusqlite::ffi::SQLITE_FULL,
                // Preserve the existing transaction rollback/escalation
                // policy pinned by pr3423_sqlite_full; #4405 is separate.
                settlement_unknown: transactional,
            })
        );
        if transactional {
            assert!(
                pool.try_writer().is_err(),
                "the existing policy retires this writer"
            );
        } else {
            pool.writer()
                .unwrap()
                .autocommit(|conn| {
                    conn.execute_batch("INSERT INTO admission_payload VALUES(NULL)")?;
                    Ok(())
                })
                .expect("definite autocommit FULL retains a usable owner");
        }
    }
}

#[test]
fn pooled_transaction_unwind_keeps_registry_and_lease_until_native_rollback() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unwind.db");
    let locks = dir.path().join("locks");
    let pool = pool(&path, &locks);
    pool.writer()
        .unwrap()
        .autocommit(|conn| {
            conn.execute_batch("CREATE TABLE pooled_owner_probe(value INTEGER)")?;
            Ok(())
        })
        .unwrap();
    let identity = VolumeIdentity::resolve(&path).unwrap();
    let lock_path = locks.join(identity.lock_filename());
    let origin = match pool.origin() {
        TxOrigin::Database(identity) => {
            khive_storage::tx_registry::TxOriginFilter::Secondary(identity)
        }
        other => panic!("expected database transaction origin, got {other:?}"),
    };
    let rollback_saw_registry = Arc::new(AtomicBool::new(false));
    let rollback_saw_lease = Arc::new(AtomicBool::new(false));
    let registry = Arc::clone(&rollback_saw_registry);
    let lease = Arc::clone(&rollback_saw_lease);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.writer()
            .unwrap()
            .transaction(|conn| -> Result<(), SqliteError> {
                conn.execute_batch("INSERT INTO pooled_owner_probe VALUES(1)")?;
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Transaction {
                            operation: TransactionOperation::Rollback
                        }
                    ) {
                        registry.store(
                            khive_storage::tx_registry::oldest_for(&origin).is_some_and(|span| {
                                span.label.as_deref() == Some("writer_guard_tx")
                            }),
                            Ordering::SeqCst,
                        );
                        let held = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&lock_path)
                            .ok()
                            .is_some_and(|file| {
                                matches!(
                                    fs4::FileExt::try_lock(&file),
                                    Err(fs4::TryLockError::WouldBlock)
                                )
                            });
                        lease.store(held, Ordering::SeqCst);
                    }
                    Authorization::Allow
                }))?;
                panic!("callback unwound inside its consuming owner");
            })
    }));
    assert!(panic.is_err());
    assert!(rollback_saw_registry.load(Ordering::SeqCst));
    assert!(rollback_saw_lease.load(Ordering::SeqCst));
    assert_eq!(count(&pool), 0);
    pool.writer()
        .unwrap()
        .transaction(|conn| {
            conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
            conn.execute_batch("INSERT INTO pooled_owner_probe VALUES(2)")?;
            Ok(())
        })
        .expect("panic rollback completed before another writer could begin");
    assert_eq!(count(&pool), 1);
}
