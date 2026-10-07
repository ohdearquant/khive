// The write-queue and write-queue-off bridge paths admit a write the same way:
// the volume lease is held while the write is probed and executed, and a write
// at or below the reserve is refused before it reaches SQLite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_on_and_queue_off_writes_hold_the_lease_and_refuse_below_the_floor() {
    use crate::disk_guard::VolumeIdentity;
    use std::sync::atomic::AtomicUsize;

    for queue_enabled in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("lease-parity.db")),
            write_queue_enabled: Some(queue_enabled),
            ..PoolConfig::for_test()
        })
        .unwrap();
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch("CREATE TABLE parity (id INTEGER PRIMARY KEY)")
            .unwrap();
        let lock_dir = pool.config().volume_lock_dir.clone().unwrap();
        let available = Arc::new(AtomicU64::new(102));
        let probes = Arc::new(AtomicUsize::new(0));
        let holding = Arc::new(AtomicUsize::new(0));
        let (sampled, counted, held) = (
            Arc::clone(&available),
            Arc::clone(&probes),
            Arc::clone(&holding),
        );
        pool.set_test_write_admission(100, move |volume| {
            counted.fetch_add(1, Ordering::SeqCst);
            // Asking for the lease again from the probing thread is refused as
            // re-entry exactly when that thread already holds it.
            let identity = VolumeIdentity::resolve(volume).unwrap();
            if matches!(
                identity.acquire(std::time::Duration::from_millis(50), Some(&lock_dir)),
                Err(crate::disk_guard::LeaseRefusal::Refused(
                    SqliteError::VolumeLeaseReentry { .. }
                ))
            ) {
                held.fetch_add(1, Ordering::SeqCst);
            }
            Ok(sampled.load(Ordering::SeqCst))
        });
        let pool = Arc::new(pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        let insert = |id: i64| SqlStatement {
            sql: format!("INSERT INTO parity VALUES ({id})"),
            params: vec![],
            label: None,
        };

        writer.execute(insert(1)).await.unwrap();
        let admitted_probes = probes.load(Ordering::SeqCst);
        assert!(admitted_probes >= 1, "queue_enabled={queue_enabled}");
        assert_eq!(
            holding.load(Ordering::SeqCst),
            admitted_probes,
            "queue_enabled={queue_enabled}: the write must be probed under the volume lease"
        );

        available.store(100, Ordering::SeqCst);
        let refused = writer
            .execute(insert(2))
            .await
            .expect_err("a write at the reserve must be refused");
        assert!(
            matches!(
                refused,
                StorageError::CapacityFloor {
                    available_bytes: 100,
                    ..
                }
            ),
            "queue_enabled={queue_enabled}: {refused:?}"
        );
        assert_eq!(
            holding.load(Ordering::SeqCst),
            probes.load(Ordering::SeqCst),
            "queue_enabled={queue_enabled}: the refusal was sampled under the lease too"
        );
        let rows: i64 = pool
            .reader()
            .unwrap()
            .conn()
            .query_row("SELECT COUNT(*) FROM parity", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "queue_enabled={queue_enabled}: refused row absent");
    }
}

fn maintenance_admission_in_child() -> bool {
    let home = tempfile::tempdir().unwrap();
    crate::test_process::run_in_child(|command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", home.path().join("sink"));
    })
}

fn maintenance_admission_pool(dir: &std::path::Path, queue_enabled: bool) -> ConnectionPool {
    let dir = dir.canonicalize().unwrap();
    ConnectionPool::new(PoolConfig {
        path: Some(dir.join("maintenance.db")),
        volume_lock_dir: Some(dir.join("volume-locks")),
        write_queue_enabled: Some(queue_enabled),
        write_queue_capacity: 8,
        write_routing_strict: false,
        write_admission_deadline_ms: 2_000,
        disk_guard_config: Some(
            crate::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
        ),
        busy_timeout: std::time::Duration::from_secs(1),
        ..PoolConfig::for_test()
    })
    .unwrap()
}

#[tokio::test]
async fn typed_checkpoint_bypasses_probe_and_lease_with_queue_on_and_off() {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    if maintenance_admission_in_child() {
        return;
    }
    for queue_enabled in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = maintenance_admission_pool(dir.path(), queue_enabled);
        pool.claim_checkpoint_ownership().unwrap();
        pool.writer().unwrap().conn().execute_batch(
            "CREATE TABLE maintenance_rows (id INTEGER PRIMARY KEY); INSERT INTO maintenance_rows VALUES (1)",
        ).unwrap();
        let lock_dir = pool.config().volume_lock_dir.clone().unwrap();
        let identity =
            crate::disk_guard::VolumeIdentity::resolve(pool.canonical_path().unwrap()).unwrap();
        let lock_file = lock_dir.join(identity.lock_filename());
        assert!(
            lock_file.is_file(),
            "setup acquired the real private volume lease"
        );
        let wal = dir.path().join("maintenance.db-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        let checkpoint_phase = Arc::new(AtomicBool::new(true));
        let probes = Arc::new(AtomicUsize::new(0));
        let (phase, observed) = (Arc::clone(&checkpoint_phase), Arc::clone(&probes));
        pool.set_test_write_admission(100, move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            assert!(
                !phase.load(Ordering::SeqCst),
                "checkpoint must not probe capacity"
            );
            Ok(0)
        });
        let pool = Arc::new(pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        assert_eq!(pool.writer_task_handle().unwrap().is_some(), queue_enabled);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        // No request has run on this handle. Setup's synchronous guards are
        // gone; acquiring any lease would recreate this private directory.
        std::fs::remove_dir_all(&lock_dir).unwrap();
        writer
            .execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)
            .await
            .unwrap();
        assert!(
            !lock_dir.exists(),
            "queue_enabled={queue_enabled}: checkpoint took a lease"
        );
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::metadata(&wal).unwrap().len(),
            0,
            "checkpoint actually truncated the WAL"
        );

        checkpoint_phase.store(false, Ordering::SeqCst);
        let error = writer
            .execute(SqlStatement {
                sql: "INSERT INTO maintenance_rows VALUES (2)".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                StorageError::CapacityFloor {
                    capability: StorageCapability::Sql,
                    available_bytes: 0,
                    floor_bytes: 100,
                    required_headroom_bytes: 0,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            lock_file.is_file(),
            "ordinary write proves the lease-creation oracle is live"
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert!(matches!(
            writer
                .query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM maintenance_rows".into(),
                    params: vec![],
                    label: None,
                })
                .await
                .unwrap(),
            Some(SqlValue::Integer(1))
        ));
        let join = pool.take_writer_task_join();
        drop(writer);
        drop(bridge);
        drop(pool);
        if let Some(join) = join {
            join.await.unwrap();
        }
    }
}

#[tokio::test]
async fn queued_vacuum_checks_copy_headroom_before_body_and_recovers() {
    use std::sync::atomic::AtomicUsize;

    if maintenance_admission_in_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut pool = maintenance_admission_pool(dir.path(), true);
    pool.writer().unwrap().conn().execute_batch(
        "CREATE TABLE maintenance_rows (id INTEGER PRIMARY KEY); INSERT INTO maintenance_rows VALUES (1)",
    ).unwrap();
    let path = pool.canonical_path().unwrap().to_path_buf();
    let headroom = crate::vacuum_capacity::estimate_vacuum_headroom(&path).unwrap();
    assert!(headroom > 0);
    let threshold = 100_u64.checked_add(headroom).unwrap();
    let available = Arc::new(AtomicU64::new(100));
    let probes = Arc::new(AtomicUsize::new(0));
    let (sampled, observed) = (Arc::clone(&available), Arc::clone(&probes));
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(sampled.load(Ordering::SeqCst))
    });
    let pool = Arc::new(pool);
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let handle = pool
        .writer_task_handle()
        .unwrap()
        .expect("queue-on route must be live");
    let bodies = Arc::new(AtomicUsize::new(0));
    for bytes in [100, threshold - 1, threshold] {
        available.store(bytes, Ordering::SeqCst);
        let public = writer
            .execute_script_top_level(TopLevelMaintenance::Vacuum)
            .await;
        let ran = Arc::clone(&bodies);
        let direct = handle
            .send_vacuum_bounded(move |_| {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StorageError>(())
            })
            .await;
        for result in [public, direct] {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, StorageError::CapacityFloor {
                capability: StorageCapability::Sql, available_bytes, floor_bytes: 100,
                required_headroom_bytes, ..
            } if *available_bytes == bytes && *required_headroom_bytes == headroom),
                "{error:?}"
            );
        }
        assert_eq!(
            bodies.load(Ordering::SeqCst),
            0,
            "a refused VACUUM body must not run"
        );
    }
    assert_eq!(probes.load(Ordering::SeqCst), 6);
    available.store(threshold + 1, Ordering::SeqCst);
    let ran = Arc::clone(&bodies);
    handle
        .send_vacuum_bounded(move |conn| {
            assert!(conn.is_autocommit(), "VACUUM must run outside BEGIN");
            ran.fetch_add(1, Ordering::SeqCst);
            Ok::<_, StorageError>(())
        })
        .await
        .unwrap();
    assert_eq!(
        bodies.load(Ordering::SeqCst),
        1,
        "the closure counter has a positive control"
    );
    writer
        .execute_script_top_level(TopLevelMaintenance::Vacuum)
        .await
        .unwrap();
    assert_eq!(probes.load(Ordering::SeqCst), 8);
    available.store(u64::MAX, Ordering::SeqCst);
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO maintenance_rows VALUES (2)".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        writer
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM maintenance_rows".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(2))
    ));
    let join = pool.take_writer_task_join().unwrap();
    drop(handle);
    drop(writer);
    drop(bridge);
    drop(pool);
    join.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn queued_vacuum_missing_estimate_refuses_before_probe_or_body_and_recovers() {
    use std::sync::atomic::AtomicUsize;

    struct RestoreDatabase {
        original: std::path::PathBuf,
        moved: std::path::PathBuf,
    }
    impl Drop for RestoreDatabase {
        fn drop(&mut self) {
            std::fs::rename(&self.moved, &self.original)
                .expect("restore private database even on unwind");
        }
    }

    if maintenance_admission_in_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut pool = maintenance_admission_pool(dir.path(), true);
    pool.writer().unwrap().conn().execute_batch(
        "CREATE TABLE maintenance_rows (id INTEGER PRIMARY KEY); INSERT INTO maintenance_rows VALUES (1)",
    ).unwrap();
    let path = pool.canonical_path().unwrap().to_path_buf();
    let probes = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&probes);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(u64::MAX)
    });
    let pool = Arc::new(pool);
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let handle = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer must open before the path is hidden");
    let moved = dir.path().join("temporarily-moved.db");
    std::fs::rename(&path, &moved).unwrap();
    let restore = RestoreDatabase {
        original: path.clone(),
        moved,
    };
    assert!(!path.exists());
    let bodies = Arc::new(AtomicUsize::new(0));
    let public = writer
        .execute_script_top_level(TopLevelMaintenance::Vacuum)
        .await;
    let ran = Arc::clone(&bodies);
    let direct = handle
        .send_vacuum_bounded(move |_| {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok::<_, StorageError>(())
        })
        .await;
    drop(restore);
    assert!(path.is_file());
    for result in [public, direct] {
        let error = result.unwrap_err();
        assert!(
            matches!(&error, StorageError::CapacityUnavailable {
            capability: StorageCapability::Sql, phase: khive_storage::CapacityUnavailablePhase::Probe,
            message, ..
        } if message.contains("cannot estimate VACUUM headroom from main database")),
            "{error:?}"
        );
    }
    assert_eq!(
        probes.load(Ordering::SeqCst),
        0,
        "estimate failure precedes the free-space probe"
    );
    assert_eq!(bodies.load(Ordering::SeqCst), 0);
    let ran = Arc::clone(&bodies);
    handle
        .send_vacuum_bounded(move |conn| {
            assert!(conn.is_autocommit());
            ran.fetch_add(1, Ordering::SeqCst);
            Ok::<_, StorageError>(())
        })
        .await
        .unwrap();
    assert_eq!(bodies.load(Ordering::SeqCst), 1);
    writer
        .execute_script_top_level(TopLevelMaintenance::Vacuum)
        .await
        .unwrap();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO maintenance_rows VALUES (2)".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        writer
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM maintenance_rows".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(2))
    ));
    assert_eq!(probes.load(Ordering::SeqCst), 3);
    let join = pool.take_writer_task_join().unwrap();
    drop(handle);
    drop(writer);
    drop(bridge);
    drop(pool);
    join.await.unwrap();
}
