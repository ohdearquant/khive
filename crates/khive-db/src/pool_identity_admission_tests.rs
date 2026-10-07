use super::*;
use std::sync::atomic::AtomicUsize;

/// A 100 ms lease deadline is only meaningful in the test's own lock
/// namespace; in the shared one it queues behind other tests' writers.
fn fixture_pool(path: &Path, volume_locks: &Path) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path.to_path_buf()),
        volume_lock_dir: Some(volume_locks.to_path_buf()),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(
            crate::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
        ),
        ..PoolConfig::for_test()
    })
    .unwrap()
}

fn assert_identity_refusal<T>(result: Result<T, SqliteError>) {
    assert!(
        matches!(
            result,
            Err(SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Identity,
                ..
            })
        ),
        "ADMISSION_VOLUME_BINDING: a changed physical volume must be refused"
    );
}

#[test]
fn acquired_volume_cannot_be_replaced_before_capacity_probe() {
    let dir = tempfile::tempdir().unwrap();
    let pool = fixture_pool(
        &dir.path().join("lease-probe.db"),
        &dir.path().join("volume-locks"),
    );
    let admission = pool.write_admission();
    let captured = admission.captured_volume_for_test().unwrap();
    let lease = admission.acquire().unwrap().expect("real volume lease");
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&probes);
    admission.set_test_space_probe(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(u64::MAX)
    });
    admission.set_test_current_volume(Some(captured.different_volume_for_test()));
    assert_identity_refusal(admission.check());
    assert_identity_refusal(admission.acquire());
    assert_eq!(
        probes.load(Ordering::SeqCst),
        0,
        "wrong volume must not be probed"
    );
    admission.set_test_current_volume(None);
    admission.check().unwrap();
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    drop(lease);
}

#[test]
fn volume_change_during_capacity_probe_refuses_the_sample() {
    let dir = tempfile::tempdir().unwrap();
    let pool = fixture_pool(
        &dir.path().join("probe-race.db"),
        &dir.path().join("volume-locks"),
    );
    let admission = pool.write_admission();
    let captured = admission.captured_volume_for_test().unwrap();
    let expected_probe_path = captured.probe_path().to_path_buf();
    let weak = Arc::downgrade(&admission);
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&probes);
    admission.set_test_space_probe(move |path| {
        assert_eq!(path, expected_probe_path);
        seen.fetch_add(1, Ordering::SeqCst);
        weak.upgrade()
            .unwrap()
            .set_test_current_volume(Some(captured.different_volume_for_test()));
        Ok(u64::MAX)
    });
    let lease = admission.acquire().unwrap().expect("real volume lease");
    assert_identity_refusal(admission.check());
    assert_eq!(
        probes.load(Ordering::SeqCst),
        1,
        "oracle changes identity inside the real probe seam"
    );
    admission.set_test_current_volume(None);
    drop(lease);
}

#[cfg(unix)]
#[test]
fn pooled_checkout_refuses_directory_replacement_on_the_same_volume() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let retained = dir.path().join("retained");
    let replacement = dir.path().join("replacement");
    std::fs::create_dir(&original).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    let path = original.join("database.db");
    let pool = fixture_pool(&path, &dir.path().join("volume-locks"));
    pool.writer()
        .unwrap()
        .execute_batch(
            "CREATE TABLE retained_row (id INTEGER); INSERT INTO retained_row VALUES (1)",
        )
        .unwrap();
    let other = Connection::open(replacement.join("database.db")).unwrap();
    other
        .execute_batch(
            "CREATE TABLE replacement_row (id INTEGER); INSERT INTO replacement_row VALUES (2)",
        )
        .unwrap();
    let before = VolumeIdentity::resolve(&path).unwrap();
    std::fs::rename(&original, &retained).unwrap();
    std::os::unix::fs::symlink(&replacement, &original).unwrap();
    assert_eq!(
        before,
        VolumeIdentity::resolve(&path).unwrap(),
        "fixture must keep the physical volume equal"
    );
    let writer = pool.writer();
    assert!(
        matches!(
            writer,
            Err(SqliteError::InvalidData(ref message))
                if message.contains("file identity changed")
        ),
        "POOLED_OPENED_FILE_BINDING: changed directory target must refuse checkout"
    );
    let bounded = pool.writer_until_for_admitted_operation(|| false);
    assert!(
        matches!(
            bounded,
            Err(SqliteError::InvalidData(ref message))
                if message.contains("file identity changed")
        ),
        "bounded checkout must preserve the same opened-file refusal"
    );
    std::fs::remove_file(&original).unwrap();
    std::fs::rename(&retained, &original).unwrap();
    assert_eq!(
        pool.writer()
            .unwrap()
            .query_row("SELECT id FROM retained_row", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        other
            .query_row("SELECT id FROM replacement_row", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[cfg(unix)]
#[test]
#[ignore = "needs a writable KHIVE_TEST_OTHER_VOLUME_DIR on a different physical volume"]
fn pooled_checkout_refuses_directory_redirect_to_another_physical_volume() {
    let dir = tempfile::tempdir().unwrap();
    let other_root =
        std::env::var_os("KHIVE_TEST_OTHER_VOLUME_DIR").expect("explicit second volume fixture");
    let source_volume = VolumeIdentity::resolve(dir.path()).unwrap();
    assert_ne!(
        source_volume,
        VolumeIdentity::resolve(Path::new(&other_root)).unwrap(),
        "fixture must be on a distinct physical volume"
    );
    let other_dir = tempfile::tempdir_in(&other_root).unwrap();
    let original = dir.path().join("original");
    let retained = dir.path().join("retained");
    std::fs::create_dir(&original).unwrap();
    let pool = fixture_pool(
        &original.join("database.db"),
        &dir.path().join("volume-locks"),
    );
    pool.writer()
        .unwrap()
        .execute_batch(
            "CREATE TABLE retained_row (id INTEGER); INSERT INTO retained_row VALUES (1)",
        )
        .unwrap();
    let other = Connection::open(other_dir.path().join("database.db")).unwrap();
    other
        .execute_batch(
            "CREATE TABLE replacement_row (id INTEGER); INSERT INTO replacement_row VALUES (2)",
        )
        .unwrap();
    std::fs::rename(&original, &retained).unwrap();
    std::os::unix::fs::symlink(other_dir.path(), &original).unwrap();
    assert_identity_refusal(pool.writer());
    std::fs::remove_file(&original).unwrap();
    std::fs::rename(&retained, &original).unwrap();
    assert_eq!(
        pool.writer()
            .unwrap()
            .query_row("SELECT id FROM retained_row", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        other
            .query_row("SELECT id FROM replacement_row", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    println!("CROSS_VOLUME_DIRECTORY_REFUSAL_COMPLETE");
}

#[test]
fn nested_checkout_on_one_thread_fails_at_once_naming_both_sites() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("reentry.db")),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(
            crate::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(5_000))
                .unwrap(),
        ),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let held = pool.writer().unwrap();
    let started = Instant::now();
    let nested = [
        pool.writer().err().expect("nested checkout is refused"),
        pool.autocommit_write_unit()
            .err()
            .expect("nested write unit is refused"),
    ];
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "re-entry waited {:?} of a 5 s lease deadline",
        started.elapsed()
    );
    for error in nested {
        let (holder_site, requester_site) = match &error {
            SqliteError::VolumeLeaseReentry {
                holder_site,
                requester_site,
            } => (holder_site.clone(), requester_site.clone()),
            other => panic!("re-entry must not read as lock contention: {other}"),
        };
        assert!(holder_site.contains(file!()), "{holder_site}");
        assert!(requester_site.contains(file!()), "{requester_site}");
        assert_ne!(holder_site, requester_site);
    }
    drop(held);
    pool.writer().expect("the lease is free after the holder");
}

fn admission_with_probe(
    dir: &Path,
    database: &str,
    available: u64,
    lock_dir: &Path,
) -> WriteAdmission {
    let admission = WriteAdmission::new(
        Some(dir.join(database)),
        0,
        DEFAULT_DISK_GUARD_DEADLINE_MS,
        Some(lock_dir.to_path_buf()),
    )
    .unwrap();
    admission.set_test_space_probe(move |_| Ok(available));
    admission
}

#[test]
fn zero_reserve_still_compares_operation_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let locks = dir.path().join("locks");
    let admission = admission_with_probe(dir.path(), "headroom.db", 1_000, &locks);
    assert!(matches!(
        admission.check_with_headroom(1_000),
        Err(SqliteError::CapacityFloor {
            available_bytes: 1_000,
            floor_bytes: 0,
            required_headroom_bytes: 1_000,
            ..
        })
    ));
    assert!(matches!(
        admission.check_with_headroom(u64::MAX),
        Err(SqliteError::CapacityFloor { .. })
    ));
    admission
        .check_with_headroom(999)
        .expect("one byte above the headroom is admitted");
}

#[test]
fn zero_reserve_without_headroom_admits_any_space() {
    let dir = tempfile::tempdir().unwrap();
    let locks = dir.path().join("locks");
    let admission = admission_with_probe(dir.path(), "no-headroom.db", 0, &locks);
    admission.check().expect("no floor term and no headroom");
    admission.check_with_headroom(0).unwrap();
}

#[test]
fn missing_vacuum_estimate_refuses_even_under_a_zero_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let locks = dir.path().join("locks");
    // The database file does not exist, so its copy-sized headroom cannot be
    // estimated, and plenty of space must not turn that into an admission.
    let admission = admission_with_probe(dir.path(), "never-created.db", u64::MAX, &locks);
    assert!(matches!(
        admission.check_for_vacuum(),
        Err(SqliteError::CapacityUnavailable {
            phase: CapacityUnavailablePhase::Probe,
            ..
        })
    ));
    admission
        .check()
        .expect("an ordinary write is not asked for the estimate");
}
