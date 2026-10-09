use super::*;

fn config(dir: &tempfile::TempDir) -> PoolConfig {
    PoolConfig {
        path: Some(dir.path().join("readers.db")),
        max_readers: 1,
        reader_max_age: Duration::from_secs(2),
        reader_max_ops: 5000,
        checkout_timeout: Duration::from_millis(10),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(crate::disk_guard_config::EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            ..Default::default()
        }),
        volume_lock_dir: Some(dir.path().join("volume-locks")),
        ..PoolConfig::for_test()
    }
}

fn state(reader: &ReaderGuard<'_>) -> (Instant, u64) {
    match reader.lease.as_ref().unwrap() {
        ReaderLease::Pooled(reader) => (reader.opened_at, reader.checkouts),
        ReaderLease::Shared(_) => panic!("expected a dedicated reader"),
    }
}

fn age(reader: &mut ReaderGuard<'_>, elapsed: Duration) {
    match reader.lease.as_mut().unwrap() {
        ReaderLease::Pooled(reader) => {
            reader.opened_at = Instant::now()
                .checked_sub(elapsed)
                .expect("the monotonic clock is older than the simulated reader age");
        }
        ReaderLease::Shared(_) => panic!("expected a dedicated reader"),
    }
}

#[test]
fn third_return_recycles_at_two_checkouts_and_resets_the_new_connection() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        reader_max_ops: 2,
        ..config(&dir)
    })
    .unwrap();
    for checkout in 1..=3 {
        let reader = pool.reader().unwrap();
        assert_eq!(state(&reader).1, checkout);
        for _ in 0..4 {
            assert_eq!(
                reader
                    .query_row("SELECT 7", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                7
            );
        }
        drop(reader);
        let counters = pool.reader_acquisition_snapshot();
        assert_eq!(counters.reader_discards, u64::from(checkout == 3));
        assert_eq!(counters.completed_pooled_checkouts, checkout);
        assert_eq!(counters.active_pooled_checkouts, 0);
        assert_eq!(counters.available_reader_admission_slots, 1);
        assert_eq!(pool.available_readers(), 1);
    }
    let replacement = pool.reader().unwrap();
    assert_eq!(state(&replacement).1, 1);
    assert_eq!(
        replacement
            .query_row("SELECT 9", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        9
    );
    drop(replacement);
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 1);
}

#[test]
fn default_limits_keep_connections_and_checkout_counts_are_per_connection() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(config(&dir)).unwrap();
    for _ in 0..3 {
        drop(pool.reader().unwrap());
    }
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 0);
    drop(pool);

    let pool = ConnectionPool::new(PoolConfig {
        max_readers: 2,
        reader_max_ops: 1,
        ..config(&dir)
    })
    .unwrap();
    let first = pool.reader().unwrap();
    let second = pool.reader().unwrap();
    assert_eq!(state(&first).1, 1);
    assert_eq!(state(&second).1, 1);
    drop(first);
    drop(second);
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 0);
    drop(pool.reader().unwrap());
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 1);
    drop(pool.reader().unwrap());
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 2);
    assert_eq!(pool.available_readers(), 2);
}

#[test]
fn age_is_strict_and_connection_lifetime_is_not_the_current_hold() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(config(&dir)).unwrap();
    let mut reader = pool.reader().unwrap();
    let (opened_at, _) = state(&reader);
    let ReaderLease::Pooled(conn) = reader.lease.as_ref().unwrap() else {
        unreachable!()
    };
    assert!(!conn.past_limit(pool.config(), opened_at + Duration::from_secs(2)));
    assert!(conn.past_limit(
        pool.config(),
        opened_at + Duration::from_secs(2) + Duration::from_nanos(1)
    ));
    let zero = PoolConfig {
        reader_max_age: Duration::ZERO,
        ..config(&dir)
    };
    assert!(!conn.past_limit(&zero, opened_at));
    assert!(conn.past_limit(&zero, opened_at + Duration::from_nanos(1)));

    age(&mut reader, Duration::from_secs(3));
    let before_return = Instant::now();
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 0);
    assert_eq!(
        reader
            .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(reader);
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 1);
    let replacement = pool.reader().unwrap();
    assert!(state(&replacement).0 >= before_return);
    assert_eq!(state(&replacement).1, 1);
    drop(replacement);
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 1);
}

#[test]
fn refusals_do_not_count_and_expired_discard_is_accounted_once() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        reader_max_ops: 1,
        ..config(&dir)
    })
    .unwrap();
    assert!(pool.reader_until(|| true).unwrap().is_none());
    let reader = pool.reader().unwrap();
    assert_eq!(state(&reader).1, 1);
    assert!(pool.reader().is_err());
    assert!(reader
        .query_row("SELECT missing_column", [], |r| r.get::<_, i64>(0))
        .is_err());
    drop(reader);
    assert_eq!(pool.reader_acquisition_snapshot().reader_discards, 0);
    let mut reader = pool.reader().unwrap();
    age(&mut reader, Duration::from_secs(3));
    reader.discard();
    drop(reader);
    let counters = pool.reader_acquisition_snapshot();
    assert_eq!(counters.pooled_checkouts, 2);
    assert_eq!(counters.completed_pooled_checkouts, 2);
    assert_eq!(counters.reader_discards, 1);
    assert_eq!(counters.reader_replacement_open_failures, 0);
    assert_eq!(counters.available_reader_admission_slots, 1);
    assert_eq!(state(&pool.reader().unwrap()).1, 1);
}

#[test]
fn zero_ops_recycles_each_return_but_shared_memory_state_is_never_recycled() {
    let dir = tempfile::tempdir().unwrap();
    let file = ConnectionPool::new(PoolConfig {
        reader_max_ops: 0,
        ..config(&dir)
    })
    .unwrap();
    for expected in 1..=3 {
        drop(file.reader().unwrap());
        assert_eq!(file.reader_acquisition_snapshot().reader_discards, expected);
        assert_eq!(file.available_readers(), 1);
    }
    let memory = ConnectionPool::new(PoolConfig {
        path: None,
        reader_max_age: Duration::ZERO,
        reader_max_ops: 0,
        ..config(&dir)
    })
    .unwrap();
    memory
        .writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE kept(value); INSERT INTO kept VALUES (42)")
        .unwrap();
    for _ in 0..3 {
        let reader = memory.reader().unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM kept", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            42
        );
    }
    let counters = memory.reader_acquisition_snapshot();
    assert_eq!(counters.completed_pooled_checkouts, 3);
    assert_eq!(counters.reader_discards, 0);
    assert_eq!(counters.available_reader_admission_slots, 1);
}

#[cfg(unix)]
#[test]
fn expired_reader_failed_refill_returns_admission_and_records_capacity_loss() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        reader_max_ops: 0,
        ..config(&dir)
    })
    .unwrap();
    let reader = pool.reader().unwrap();
    std::fs::remove_file(dir.path().join("readers.db")).unwrap();
    drop(reader);
    let counters = pool.reader_acquisition_snapshot();
    assert_eq!(counters.reader_discards, 1);
    assert_eq!(counters.reader_replacement_open_failures, 1);
    assert_eq!(counters.completed_pooled_checkouts, 1);
    assert_eq!(counters.active_pooled_checkouts, 0);
    assert_eq!(counters.available_reader_admission_slots, 1);
    assert_eq!(pool.available_readers(), 0);
    assert!(pool.reader().is_err());
}

#[test]
fn recycling_environment_preserves_unsigned_threshold_policy() {
    if crate::test_process::run_in_child(|command| {
        command
            .env_remove("KHIVE_READER_MAX_AGE_SECS")
            .env_remove("KHIVE_READER_MAX_OPS");
    }) {
        return;
    }
    assert_eq!(
        PoolConfig::default().reader_max_age,
        Duration::from_secs(300)
    );
    assert_eq!(PoolConfig::default().reader_max_ops, 5000);
    for (value, age, ops) in [
        ("0", 0, 0),
        ("2", 2, 2),
        ("invalid", 300, 5000),
        ("-1", 300, 5000),
        (" 2 ", 300, 5000),
    ] {
        crate::test_process::set_var("KHIVE_READER_MAX_AGE_SECS", value);
        crate::test_process::set_var("KHIVE_READER_MAX_OPS", value);
        assert_eq!(
            PoolConfig::default().reader_max_age,
            Duration::from_secs(age)
        );
        assert_eq!(PoolConfig::default().reader_max_ops, ops);
    }
}
