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
                let captured = fs::read(log.path()).unwrap().len();
                assert_eq!(
                    tick(store, reading, mono + Duration::from_secs(seconds + offset))
                        .await
                        .unwrap(),
                    0
                );
                present(store, &id, true);
                let output = fs::read(log.path()).unwrap();
                let tick_log = std::str::from_utf8(&output[captured..]).unwrap();
                assert_eq!(
                    tick_log.contains("upload lease clock fault"),
                    std::ptr::eq(store, &behind),
                    "diagnostics from this completed actual sweeper tick: {tick_log}"
                );
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

#[tokio::test]
async fn owner_and_sequence_rollback_restart_observation() {
    for change_owner in [false, true] {
        let (dir, owner, config) = fixture();
        let other = crate::StorageBackend::sqlite_for_test(dir.path().join("other.db")).unwrap();
        let other_id = other.database_owner_identity().unwrap().durable_id();
        assert_ne!(other_id, config.owner());
        let id = owner.begin_upload_with_lease(0, config).await.unwrap();
        owner.renew_upload(&id).await.unwrap();
        owner.renew_upload(&id).await.unwrap();
        let foreign = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
        let wall = wall(&owner, &id);
        let mono = Instant::now();
        assert_eq!(tick(&foreign, wall, mono).await.unwrap(), 0);
        assert_eq!(
            tick(&foreign, wall, mono + Duration::from_secs(359))
                .await
                .unwrap(),
            0
        );
        let upload = id.clone();
        run(&owner, "fixture_lease_replacement", move |context| {
            let directory = context.directory(false).unwrap();
            let mut value = lease::read(&directory, &upload)?.unwrap();
            assert_eq!(value.renew_seq, 2);
            if change_owner {
                value.owner = other_id;
            } else {
                value.renew_seq = 1;
            }
            lease::publish(&context, &directory, &upload, &value)
        })
        .await
        .unwrap();
        assert_eq!(
            tick(&foreign, wall, mono + Duration::from_secs(359))
                .await
                .unwrap(),
            0
        );
        let observed = foreign.upload_observations.lock().unwrap()[&id];
        assert_eq!(
            (observed.0, observed.1, observed.2),
            (
                if change_owner {
                    other_id
                } else {
                    config.owner()
                },
                if change_owner { 2 } else { 1 },
                mono + Duration::from_secs(359)
            )
        );
        assert_eq!(
            tick(&foreign, wall, mono + Duration::from_secs(718))
                .await
                .unwrap(),
            0
        );
        present(&foreign, &id, true);
        assert_eq!(
            tick(&foreign, wall, mono + Duration::from_secs(719))
                .await
                .unwrap(),
            1
        );
        present(&foreign, &id, false);
        assert!(foreign.upload_observations.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn retained_observer_survives_fresh_store_construction() {
    let (_dir, owner, config) = fixture();
    let id = owner.begin_upload_with_lease(0, config).await.unwrap();
    let retained = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
    let wall = wall(&owner, &id);
    let mono = Instant::now();
    assert_eq!(tick(&retained, wall, mono).await.unwrap(), 0);
    let original = retained.upload_observations.lock().unwrap()[&id];
    let fresh = FsBlobStore::open_existing(owner.root().to_path_buf(), 0).unwrap();
    assert_eq!(
        tick(&fresh, wall, mono + Duration::from_secs(360))
            .await
            .unwrap(),
        0
    );
    assert_eq!(retained.upload_observations.lock().unwrap()[&id], original);
    assert_eq!(
        fresh.upload_observations.lock().unwrap()[&id].2,
        mono + Duration::from_secs(360)
    );
    present(&owner, &id, true);
    assert_eq!(
        tick(&retained, wall, mono + Duration::from_secs(360))
            .await
            .unwrap(),
        1
    );
    present(&owner, &id, false);
}

#[tokio::test]
async fn recognized_stageless_leases_are_removed_without_sweeping_other_names() {
    let (_dir, store, config) = fixture();
    let directory = store.root().join(UPLOAD_DIRECTORY);
    let mono = Instant::now();
    for malformed in [false, true] {
        let id = store.begin_upload_with_lease(0, config).await.unwrap();
        let wall = wall(&store, &id);
        tick(&store, wall, mono).await.unwrap();
        assert!(store.upload_observations.lock().unwrap().contains_key(&id));
        if malformed {
            fs::write(paths(&store, &id).1, b"{malformed").unwrap();
        }
        fs::remove_file(paths(&store, &id).0).unwrap();
        let temp = directory.join(format!(".{id}.lease-{}", Uuid::new_v4()));
        let other = directory.join("repair-not-an-upload");
        fs::write(&temp, b"retained temporary").unwrap();
        fs::write(&other, b"retained foreign").unwrap();
        assert_eq!(tick(&store, wall, mono).await.unwrap(), 0);
        present(&store, &id, false);
        assert!(!store.upload_observations.lock().unwrap().contains_key(&id));
        assert_eq!(fs::read(&temp).unwrap(), b"retained temporary");
        assert_eq!(fs::read(&other).unwrap(), b"retained foreign");
    }
    #[cfg(unix)]
    {
        let id = store.begin_upload_with_lease(0, config).await.unwrap();
        let wall = wall(&store, &id);
        let sentinel = _dir.path().join("outside-orphan-lease");
        fs::write(&sentinel, b"retained outside").unwrap();
        fs::remove_file(paths(&store, &id).0).unwrap();
        fs::remove_file(paths(&store, &id).1).unwrap();
        std::os::unix::fs::symlink(&sentinel, paths(&store, &id).1).unwrap();
        assert!(tick(&store, wall, mono).await.is_err());
        assert!(!paths(&store, &id).0.exists());
        assert!(fs::symlink_metadata(paths(&store, &id).1)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&sentinel).unwrap(), b"retained outside");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_first_stage_does_not_block_healthy_sibling_expiry() {
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_MALFORMED_FIRST_CHILD",
        false,
        |_| {},
    ) {
        return;
    }
    let log = diagnostics();
    for (malformed, symlink) in [
        (b"{malformed".as_slice(), false),
        (&[b'x'; 1025][..], false),
        (b"outside sentinel".as_slice(), true),
    ] {
        let (_dir, store, config) = fixture();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(store.begin_upload_with_lease(0, config).await.unwrap());
        }
        let wall = wall(&store, &ids[0]);
        let observed_names = || {
            use std::os::fd::AsRawFd;
            let directory = fs::File::open(store.root().join(UPLOAD_DIRECTORY)).unwrap();
            read_dir_names_no_follow(directory.as_raw_fd())
                .unwrap()
                .into_iter()
                .filter_map(|name| UploadId::from_hex(name).ok())
                .collect::<Vec<_>>()
        };
        let order = observed_names();
        assert_eq!(order.len(), 3);
        let bad = &order[0];
        let sentinel = _dir.path().join("outside-lease");
        if symlink {
            fs::write(&sentinel, malformed).unwrap();
            fs::remove_file(paths(&store, bad).1).unwrap();
            std::os::unix::fs::symlink(&sentinel, paths(&store, bad).1).unwrap();
        } else {
            fs::write(paths(&store, bad).1, malformed).unwrap();
        }
        assert_eq!(
            observed_names(),
            order,
            "actual unchanged directory listing order"
        );
        assert!(
            tick(&store, wall + Duration::from_secs(86_460), Instant::now())
                .await
                .is_err()
        );
        present(&store, bad, true);
        assert_eq!(fs::read(paths(&store, bad).0).unwrap(), b"");
        assert_eq!(fs::read(paths(&store, bad).1).unwrap(), malformed);
        if symlink {
            assert!(fs::symlink_metadata(paths(&store, bad).1)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read(&sentinel).unwrap(), malformed);
        }
        for healthy in &order[1..] {
            present(&store, healthy, false);
        }
    }
    assert!(fs::read_to_string(log.path())
        .unwrap()
        .contains("upload lease sweep retained an invalid entry for repair"));
}
