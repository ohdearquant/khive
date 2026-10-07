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
