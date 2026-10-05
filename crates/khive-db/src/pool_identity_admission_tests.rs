use super::*;
use std::sync::atomic::AtomicUsize;

fn fixture_pool(path: &Path) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path.to_path_buf()),
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
    let pool = fixture_pool(&dir.path().join("lease-probe.db"));
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
    let pool = fixture_pool(&dir.path().join("probe-race.db"));
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
    let pool = fixture_pool(&path);
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
        matches!(writer, Err(SqliteError::InvalidData(ref message)) if message.contains("file identity changed")),
        "POOLED_OPENED_FILE_BINDING: changed directory target must refuse checkout"
    );
    let bounded = pool.writer_until_for_admitted_operation(|| false);
    assert!(
        matches!(bounded, Err(SqliteError::InvalidData(ref message)) if message.contains("file identity changed")),
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
#[ignore = "requires an explicit writable KHIVE_TEST_OTHER_VOLUME_DIR on a different physical volume"]
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
    let pool = fixture_pool(&original.join("database.db"));
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
