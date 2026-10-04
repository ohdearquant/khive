use super::*;
use std::time::Instant;

fn fixture() -> (tempfile::TempDir, FsBlobStore, UploadLeaseConfig) {
    let dir = tempfile::tempdir().unwrap();
    let main = crate::StorageBackend::sqlite_for_test(dir.path().join("main.db")).unwrap();
    let config = UploadLeaseConfig::new(
        main.database_owner_identity().unwrap().durable_id(),
        Duration::from_secs(60),
    )
    .unwrap();
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
    (dir, store, config)
}
fn paths(store: &FsBlobStore, id: &UploadId) -> (PathBuf, PathBuf) {
    let stage = store.root().join(UPLOAD_DIRECTORY).join(id.as_str());
    (stage.clone(), stage.with_extension("lease"))
}
fn wall(store: &FsBlobStore, id: &UploadId) -> SystemTime {
    let value: lease::Lease =
        serde_json::from_slice(&fs::read(paths(store, id).1).unwrap()).unwrap();
    SystemTime::UNIX_EPOCH + Duration::from_millis(value.renewed_at)
}
fn present(store: &FsBlobStore, id: &UploadId, expected: bool) {
    let (stage, sidecar) = paths(store, id);
    assert_eq!((stage.exists(), sidecar.exists()), (expected, expected));
}
async fn tick(store: &FsBlobStore, wall: SystemTime, monotonic: Instant) -> StorageResult<u64> {
    sweep_at(store, Some((wall, monotonic))).await
}
fn diagnostics() -> tempfile::NamedTempFile {
    let log = tempfile::NamedTempFile::new().unwrap();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(Arc::new(log.reopen().unwrap()))
            .with_ansi(false)
            .without_time()
            .finish(),
    )
    .unwrap();
    log
}

#[tokio::test]
async fn shared_root_sweep_uses_each_owner_bound() {
    let (dir, short, config) = fixture();
    let other_main = crate::StorageBackend::sqlite_for_test(dir.path().join("other.db")).unwrap();
    let long = FsBlobStore::open_existing(short.root().to_path_buf(), 0).unwrap();
    let long_config = UploadLeaseConfig::new(
        other_main.database_owner_identity().unwrap().durable_id(),
        Duration::from_secs(1000),
    )
    .unwrap();
    assert_ne!(config.owner(), long_config.owner());
    let a = short.begin_upload_with_lease(1, config).await.unwrap();
    let b = long.begin_upload_with_lease(1, long_config).await.unwrap();
    let wall = wall(&short, &a);
    let mono = Instant::now();
    assert_eq!(tick(&short, wall, mono).await.unwrap(), 0);
    assert_eq!(
        tick(&short, wall, mono + Duration::from_secs(359))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        tick(&short, wall, mono + Duration::from_secs(360))
            .await
            .unwrap(),
        1
    );
    present(&short, &a, false);
    present(&long, &b, true);
    assert_eq!(long.append_part(&b, b"x".to_vec()).await.unwrap(), 1);
    long.commit_upload(
        &b,
        &ContentRef::from_digest_bytes(blake3::hash(b"x").as_bytes()),
    )
    .await
    .unwrap();
    present(&long, &b, false);
}

#[tokio::test]
async fn foreign_unknown_owner_expires_after_own_observation() {
    let (_dir, owner, config) = fixture();
    let id = owner.begin_upload_with_lease(0, config).await.unwrap();
    let foreign = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
    assert!(foreign.upload_observations.lock().unwrap().is_empty());
    let wall = wall(&owner, &id);
    let mono = Instant::now();
    tick(&foreign, wall, mono).await.unwrap();
    assert_eq!(
        tick(&foreign, wall, mono + Duration::from_secs(359))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        tick(&foreign, wall, mono + Duration::from_secs(360))
            .await
            .unwrap(),
        1
    );
    present(&foreign, &id, false);
    assert!(foreign.upload_observations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_lease_floor_and_malformed_retention_are_distinct() {
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_LEASE_DIAGNOSTIC_CHILD",
        false,
        |_| {},
    ) {
        return;
    }
    let log = diagnostics();
    let (_dir, store, config) = fixture();
    assert!(matches!(
        store.begin_upload(0).await,
        Err(StorageError::Unsupported { .. })
    ));
    let absent = store.begin_upload_with_lease(1, config).await.unwrap();
    let malformed = store.begin_upload_with_lease(0, config).await.unwrap();
    fs::remove_file(paths(&store, &absent).1).unwrap();
    assert!(store.append_part(&absent, vec![1]).await.is_err());
    assert!(store.renew_upload(&absent).await.is_err());
    assert_eq!(fs::read(paths(&store, &absent).0).unwrap(), b"");
    let now = SystemTime::now();
    fs::write(paths(&store, &malformed).1, b"{bad lease").unwrap();
    fs::File::options()
        .write(true)
        .open(paths(&store, &absent).0)
        .unwrap()
        .set_modified(now - Duration::from_secs(3601))
        .unwrap();
    assert!(store
        .sweep_uploads(Duration::from_secs(3600))
        .await
        .is_err());
    assert!(paths(&store, &absent).0.exists());
    for id in [&absent, &malformed] {
        fs::File::options()
            .write(true)
            .open(paths(&store, id).0)
            .unwrap()
            .set_modified(now - Duration::from_secs(86_400 - 1))
            .unwrap();
    }
    let mono = Instant::now();
    assert!(tick(&store, now, mono).await.is_err());
    assert!(paths(&store, &absent).0.exists());
    assert!(tick(&store, now + Duration::from_secs(1), mono)
        .await
        .is_err());
    assert!(!paths(&store, &absent).0.exists());
    present(&store, &malformed, true);
    assert_eq!(
        fs::read(paths(&store, &malformed).1).unwrap(),
        b"{bad lease"
    );
    assert!(fs::read_to_string(log.path())
        .unwrap()
        .contains("retained an invalid entry for repair"));
    fs::remove_file(paths(&store, &malformed).0).unwrap();
    assert_eq!(tick(&store, now, mono).await.unwrap(), 0);
    present(&store, &malformed, false);
}

#[tokio::test]
async fn renewal_resets_independent_skewed_sweepers() {
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_LEASE_DIAGNOSTIC_CHILD",
        false,
        |_| {},
    ) {
        return;
    }
    let log = diagnostics();
    let (_dir, owner, config) = fixture();
    let ahead = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
    let behind = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
    for deleter in [&ahead, &behind] {
        let id = owner.begin_upload_with_lease(0, config).await.unwrap();
        let wall = wall(&owner, &id);
        let mono = Instant::now();
        for seconds in (0..=500).step_by(50) {
            if seconds != 0 {
                owner.renew_upload(&id).await.unwrap();
            }
            for (store, reading, offset) in [
                (&ahead, wall + Duration::from_secs(3600), 0),
                (&behind, wall - Duration::from_secs(3600), 10),
            ] {
                assert_eq!(
                    tick(store, reading, mono + Duration::from_secs(seconds + offset))
                        .await
                        .unwrap(),
                    0
                );
                present(store, &id, true);
            }
        }
        assert_eq!(
            tick(&ahead, wall, mono + Duration::from_secs(859))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            tick(&behind, wall, mono + Duration::from_secs(869))
                .await
                .unwrap(),
            0
        );
        let threshold = if std::ptr::eq(deleter, &ahead) {
            860
        } else {
            870
        };
        assert_eq!(
            tick(
                deleter,
                wall - Duration::from_secs(3600),
                mono + Duration::from_secs(threshold)
            )
            .await
            .unwrap(),
            1
        );
        present(deleter, &id, false);
        tick(&ahead, wall, mono + Duration::from_secs(1000))
            .await
            .unwrap();
        tick(&behind, wall, mono + Duration::from_secs(1010))
            .await
            .unwrap();
        assert!(ahead.upload_observations.lock().unwrap().is_empty());
        assert!(behind.upload_observations.lock().unwrap().is_empty());
    }
    assert!(fs::read_to_string(log.path())
        .unwrap()
        .contains("upload lease clock fault"));
}

#[tokio::test]
async fn restart_starvation_eventually_uses_wall_backstop() {
    let (_dir, owner, config) = fixture();
    let id = owner.begin_upload_with_lease(0, config).await.unwrap();
    let wall = wall(&owner, &id);
    let mono = Instant::now();
    for seconds in [0, 100, 200, 400, 800] {
        let restarted = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
        assert_eq!(
            tick(&restarted, wall, mono + Duration::from_secs(seconds))
                .await
                .unwrap(),
            0
        );
        present(&restarted, &id, true);
    }
    let restarted = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
    assert_eq!(
        tick(&restarted, wall + Duration::from_secs(86_459), mono)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        tick(&restarted, wall - Duration::from_secs(3600), mono)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        tick(&restarted, wall + Duration::from_secs(86_460), mono)
            .await
            .unwrap(),
        1
    );
    present(&restarted, &id, false);
}
