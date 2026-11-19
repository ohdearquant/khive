use super::*;

fn fixture() -> (tempfile::TempDir, FsBlobStore, UploadLeaseConfig) {
    let dir = tempfile::tempdir().unwrap();
    let main = crate::StorageBackend::sqlite_for_test(dir.path().join("main.db")).unwrap();
    let owner = main.database_owner_identity().unwrap().durable_id();
    let config = UploadLeaseConfig::new(owner, Duration::from_secs(60)).unwrap();
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
    (dir, store, config)
}
fn paths(store: &FsBlobStore, id: &UploadId) -> (PathBuf, PathBuf) {
    let stage = store.root().join(UPLOAD_DIRECTORY).join(id.as_str());
    (stage.clone(), stage.with_extension("lease"))
}
fn read(store: &FsBlobStore, id: &UploadId) -> serde_json::Value {
    serde_json::from_slice(&fs::read(paths(store, id).1).unwrap()).unwrap()
}
fn both_gone(store: &FsBlobStore, id: &UploadId) {
    let (stage, sidecar) = paths(store, id);
    assert!(!stage.exists() && !sidecar.exists());
    assert_eq!(fs::read_dir(stage.parent().unwrap()).unwrap().count(), 0);
}

#[tokio::test]
async fn leased_begin_and_parts_publish_complete_immutable_lease() {
    let (_dir, store, config) = fixture();
    assert_eq!(
        store.upload_lease_idle_cap(),
        Some(Duration::from_secs(21_600))
    );
    #[cfg(unix)]
    let hook = sync_hook::install_publication(store.root(), None);
    let id = store.begin_upload_with_lease(2, config).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            *hook.completed.lock().unwrap(),
            [
                "begin_upload_sync",
                "lease_sync_file",
                "lease_replace",
                "lease_sync_directory",
                "upload_sync_root"
            ]
        );
        let uploads = fs::metadata(store.root().join(UPLOAD_DIRECTORY)).unwrap();
        let root = fs::metadata(store.root()).unwrap();
        assert_eq!(
            *hook.directories.lock().unwrap(),
            [
                ("lease_sync_directory", uploads.dev(), uploads.ino()),
                ("upload_sync_root", root.dev(), root.ino())
            ]
        );
    }
    let initial = read(&store, &id);
    assert_eq!(initial.as_object().unwrap().len(), 4);
    assert_eq!(initial["owner"], config.owner().to_string());
    assert_eq!(initial["idle_secs"], 60);
    assert_eq!(initial["renew_seq"], 0);
    assert!(initial["renewed_at"].as_u64().unwrap() > 0);
    for (bytes, count) in [(b"a".to_vec(), 1), (b"b".to_vec(), 2)] {
        assert_eq!(store.append_part(&id, bytes).await.unwrap(), count);
        let value = read(&store, &id);
        assert_eq!(value["owner"], initial["owner"]);
        assert_eq!(value["idle_secs"], initial["idle_secs"]);
        assert_eq!(value["renew_seq"], count);
        assert!(value["renewed_at"].as_u64().unwrap() >= initial["renewed_at"].as_u64().unwrap());
    }
    let before = fs::read(paths(&store, &id).0).unwrap();
    store.renew_upload(&id).await.unwrap();
    assert_eq!(read(&store, &id)["renew_seq"], 3);
    assert_eq!(fs::read(paths(&store, &id).0).unwrap(), before);
    assert_eq!(before, b"ab");
    assert_eq!(store.sweep_uploads(Duration::ZERO).await.unwrap(), 0);
    assert!(paths(&store, &id).0.exists() && paths(&store, &id).1.exists());
    let reference = ContentRef::from_digest_bytes(blake3::hash(b"ab").as_bytes());
    store.commit_upload(&id, &reference).await.unwrap();
    assert_eq!(
        store.get_bounded_verified(&reference, 2).await.unwrap(),
        b"ab"
    );
    both_gone(&store, &id);
}

#[cfg(unix)]
#[tokio::test]
async fn lease_publication_failure_never_acknowledges_or_releases_root_early() {
    for operation in [
        "begin_upload_sync",
        "lease_sync_file",
        "lease_replace",
        "lease_sync_directory",
        "upload_sync_root",
    ] {
        let (_dir, store, config) = fixture();
        sync_hook::install_publication(store.root(), Some(operation));
        assert!(store
            .begin_upload_with_lease(1, config)
            .await
            .unwrap_err()
            .to_string()
            .contains(operation));
        assert_eq!(
            fs::read_dir(store.root().join(UPLOAD_DIRECTORY))
                .unwrap()
                .count(),
            0
        );
    }
    for operation in [
        "append_part_sync",
        "lease_sync_file",
        "lease_replace",
        "lease_sync_directory",
    ] {
        let (_dir, store, config) = fixture();
        let id = store.begin_upload_with_lease(1, config).await.unwrap();
        sync_hook::install_publication(store.root(), Some(operation));
        assert!(store
            .append_part(&id, vec![7])
            .await
            .unwrap_err()
            .to_string()
            .contains(operation));
        assert_eq!(fs::read(paths(&store, &id).0).unwrap(), [7]);
        assert_eq!(read(&store, &id)["owner"], config.owner().to_string());
        store.abort_upload(&id).await.unwrap();
        both_gone(&store, &id);
    }
    let (_dir, store, config) = fixture();
    let store = Arc::new(store);
    let id = store.begin_upload_with_lease(1, config).await.unwrap();
    let hook = sync_hook::install_publication(store.root(), None);
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    hook.on_step("lease_replace", move || {
        reached_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let writer = store.clone();
    let upload = id.clone();
    let task = tokio::spawn(async move { writer.append_part(&upload, vec![1]).await });
    tokio::task::spawn_blocking(move || reached_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(store.write_lock.try_lock().is_err());
    let other = FsBlobStore::open_existing(store.root().to_owned(), 0).unwrap();
    let mut sweep = tokio::spawn(async move { other.sweep_uploads(Duration::ZERO).await });
    assert!(tokio::time::timeout(Duration::from_millis(25), &mut sweep)
        .await
        .is_err());
    release_tx.send(()).unwrap();
    assert_eq!(sweep.await.unwrap().unwrap(), 0);
    assert_eq!(read(&store, &id)["renew_seq"], 1);
    assert_eq!(fs::read(paths(&store, &id).0).unwrap(), [1]);
    assert_eq!(
        *hook.completed.lock().unwrap(),
        [
            "append_part_sync",
            "lease_sync_file",
            "lease_replace",
            "lease_sync_directory"
        ]
    );
    store.abort_upload(&id).await.unwrap();
}

#[tokio::test]
async fn leased_commit_and_abort_remove_both_siblings() {
    let (_dir, store, config) = fixture();
    for dedup in [false, true] {
        let bytes = if dedup {
            b"duplicate".as_slice()
        } else {
            b"fresh".as_slice()
        };
        let reference = ContentRef::from_digest_bytes(blake3::hash(bytes).as_bytes());
        if dedup {
            store.put(bytes.to_vec()).await.unwrap();
        }
        let id = store
            .begin_upload_with_lease(bytes.len() as u64, config)
            .await
            .unwrap();
        store.append_part(&id, bytes.to_vec()).await.unwrap();
        store.commit_upload(&id, &reference).await.unwrap();
        both_gone(&store, &id);
        assert_eq!(
            store
                .get_bounded_verified(&reference, bytes.len() as u64)
                .await
                .unwrap(),
            bytes
        );
    }
    let id = store.begin_upload_with_lease(0, config).await.unwrap();
    store.abort_upload(&id).await.unwrap();
    store.abort_upload(&id).await.unwrap();
    both_gone(&store, &id);
    #[cfg(unix)]
    for operation in ["put_fsync", "lease_unlink", "lease_sync_cleanup"] {
        let id = store.begin_upload_with_lease(1, config).await.unwrap();
        store.append_part(&id, vec![1]).await.unwrap();
        let reference = ContentRef::from_digest_bytes(blake3::hash(&[1]).as_bytes());
        let before = fs::read(paths(&store, &id).1).unwrap();
        sync_hook::install_publication(store.root(), Some(operation));
        assert!(store
            .commit_upload(&id, &reference)
            .await
            .unwrap_err()
            .to_string()
            .contains(operation));
        if operation == "lease_sync_cleanup" {
            assert!(!paths(&store, &id).1.exists());
        } else {
            assert!(
                paths(&store, &id).1.is_file(),
                "lease must survive {operation}"
            );
            assert_eq!(fs::read(paths(&store, &id).1).unwrap(), before);
        }
        assert_eq!(paths(&store, &id).0.exists(), operation == "put_fsync");
        let retry = sync_hook::install_publication(store.root(), None);
        store.abort_upload(&id).await.unwrap();
        assert!(retry
            .completed
            .lock()
            .unwrap()
            .contains(&"lease_sync_cleanup"));
        both_gone(&store, &id);
    }
}

#[tokio::test]
async fn leased_validation_and_floor_fail_before_renewal() {
    let (_dir, store, config) = fixture();
    assert!(UploadLeaseConfig::new(config.owner(), Duration::ZERO).is_err());
    assert!(UploadLeaseConfig::new(config.owner(), Duration::from_nanos(1)).is_err());
    let too_long = UploadLeaseConfig::new(config.owner(), Duration::from_secs(21_601)).unwrap();
    assert!(store.begin_upload_with_lease(1, too_long).await.is_err());
    let id = store.begin_upload_with_lease(1, config).await.unwrap();
    let (stage, sidecar) = paths(&store, &id);
    let original = fs::read(&sidecar).unwrap();
    let high_floor = FsBlobStore::open_existing(store.root().to_owned(), u64::MAX).unwrap();
    assert!(matches!(
        high_floor.renew_upload(&id).await,
        Err(StorageError::CapacityFloor { .. })
    ));
    assert_eq!(fs::read(&sidecar).unwrap(), original);
    assert_eq!(fs::read(&stage).unwrap(), [0u8; 0]);
    for (field, value) in [
        ("owner", serde_json::json!("not-uuid")),
        ("idle_secs", serde_json::json!(0)),
        ("idle_secs", serde_json::json!(21_601)),
        ("renew_seq", serde_json::json!(u64::MAX)),
        ("renewed_at", serde_json::json!(-1)),
        ("unknown", serde_json::json!(true)),
    ] {
        let mut lease = read(&store, &id);
        lease[field] = value;
        let bytes = serde_json::to_vec(&lease).unwrap();
        fs::write(&sidecar, &bytes).unwrap();
        assert!(store.append_part(&id, vec![1]).await.is_err());
        assert!(store.renew_upload(&id).await.is_err());
        assert_eq!(fs::read(&stage).unwrap(), [0u8; 0]);
        assert_eq!(fs::read(&sidecar).unwrap(), bytes);
        fs::write(&sidecar, &original).unwrap();
    }
    fs::write(&sidecar, b"malformed").unwrap();
    assert!(store.sweep_uploads(Duration::ZERO).await.is_err());
    assert!(stage.exists());
    fs::write(&sidecar, &original).unwrap();
    #[cfg(unix)]
    {
        let sentinel = _dir.path().join("outside-lease");
        fs::write(&sentinel, &original).unwrap();
        fs::remove_file(&sidecar).unwrap();
        std::os::unix::fs::symlink(&sentinel, &sidecar).unwrap();
        assert!(store.append_part(&id, vec![1]).await.is_err());
        assert!(store.renew_upload(&id).await.is_err());
        assert!(store.sweep_uploads(Duration::ZERO).await.is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), original);
        assert_eq!(fs::read(&stage).unwrap(), [0u8; 0]);
        fs::remove_file(&sidecar).unwrap();
        fs::write(&sidecar, &original).unwrap();
    }
    store.abort_upload(&id).await.unwrap();
    both_gone(&store, &id);
}

#[tokio::test]
async fn nil_config_and_stored_owner_refuse_independently() {
    let (_dir, store, config) = fixture();
    assert!(matches!(
        UploadLeaseConfig::new(Uuid::nil(), Duration::from_secs(60)),
        Err(StorageError::InvalidInput { .. })
    ));
    assert!(!store.root().join(UPLOAD_DIRECTORY).exists());
    let id = store.begin_upload_with_lease(1, config).await.unwrap();
    let mut value = read(&store, &id);
    value["owner"] = serde_json::json!(Uuid::nil());
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(paths(&store, &id).1, &bytes).unwrap();
    assert!(store.append_part(&id, vec![1]).await.is_err());
    assert!(store.renew_upload(&id).await.is_err());
    assert_eq!(fs::read(paths(&store, &id).0).unwrap(), b"");
    assert_eq!(fs::read(paths(&store, &id).1).unwrap(), bytes);
    store.abort_upload(&id).await.unwrap();
    both_gone(&store, &id);
}

#[tokio::test]
async fn timestamp_deadline_overflow_refuses_before_bytes_or_renewal() {
    let (_dir, store, config) = fixture();
    let id = store.begin_upload_with_lease(1, config).await.unwrap();
    let mut value = read(&store, &id);
    value["renewed_at"] = serde_json::json!(u64::MAX);
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(paths(&store, &id).1, &bytes).unwrap();
    let upload = id.clone();
    assert!(run(&store, "fixture_timestamp_read", move |context| {
        lease::read(&context.directory(false).unwrap(), &upload)
    })
    .await
    .is_err());
    assert!(store.append_part(&id, vec![1]).await.is_err());
    assert!(store.renew_upload(&id).await.is_err());
    assert_eq!(fs::read(paths(&store, &id).0).unwrap(), b"");
    assert_eq!(fs::read(paths(&store, &id).1).unwrap(), bytes);
    store.abort_upload(&id).await.unwrap();
}

#[tokio::test]
async fn terminal_sequence_refuses_wrapping_but_remains_reapable() {
    let (_dir, store, config) = fixture();
    let id = store.begin_upload_with_lease(1, config).await.unwrap();
    let mut value = read(&store, &id);
    for bad in [serde_json::json!(-1), serde_json::json!(0.5)] {
        value["renew_seq"] = bad;
        assert!(serde_json::from_value::<lease::Lease>(value.clone()).is_err());
    }
    value["renew_seq"] = serde_json::json!(u64::MAX);
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(paths(&store, &id).1, &bytes).unwrap();
    let upload = id.clone();
    assert_eq!(
        run(&store, "fixture_terminal_read", move |context| {
            Ok(lease::read(&context.directory(false).unwrap(), &upload)?
                .unwrap()
                .renew_seq)
        })
        .await
        .unwrap(),
        u64::MAX
    );
    assert!(store.append_part(&id, vec![1]).await.is_err());
    assert!(store.renew_upload(&id).await.is_err());
    assert_eq!(fs::read(paths(&store, &id).0).unwrap(), b"");
    assert_eq!(fs::read(paths(&store, &id).1).unwrap(), bytes);
    let wall =
        SystemTime::UNIX_EPOCH + Duration::from_millis(value["renewed_at"].as_u64().unwrap());
    let mono = std::time::Instant::now();
    assert_eq!(sweep_at(&store, Some((wall, mono))).await.unwrap(), 0);
    assert_eq!(
        sweep_at(
            &store,
            Some((wall, mono + Duration::from_secs(config.idle_secs() + 300)))
        )
        .await
        .unwrap(),
        1
    );
    both_gone(&store, &id);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_begin_preserves_primary_error_and_reports_cleanup_failure() {
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_BEGIN_CLEANUP_CHILD",
        false,
        |_| {},
    ) {
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(Arc::new(log.reopen().unwrap()))
            .with_ansi(false)
            .without_time()
            .finish(),
    )
    .unwrap();
    let (_dir, store, config) = fixture();
    let hook = sync_hook::install_publication(store.root(), Some("begin_upload_sync"));
    let error = store.begin_upload_with_lease(1, config).await.unwrap_err();
    assert!(matches!(error, StorageError::Driver { operation, .. }
        if operation == "begin_upload_sync"));
    assert_eq!(
        fs::read_dir(store.root().join(UPLOAD_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(*hook.completed.lock().unwrap(), ["lease_sync_cleanup"]);
    let high = FsBlobStore::open_existing(store.root().to_path_buf(), u64::MAX).unwrap();
    sync_hook::install_publication(high.root(), Some("lease_sync_cleanup"));
    assert!(matches!(
        high.begin_upload_with_lease(1, config).await,
        Err(StorageError::CapacityFloor { .. })
    ));
    assert_eq!(
        fs::read_dir(high.root().join(UPLOAD_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
    assert!(fs::read_to_string(log.path())
        .unwrap()
        .contains("failed upload begin cleanup; original begin error retained"));
    let retry = sync_hook::install_publication(high.root(), None);
    high.abort_upload(&UploadId::from_bytes(&[9; 16]))
        .await
        .unwrap();
    assert!(retry
        .completed
        .lock()
        .unwrap()
        .contains(&"lease_sync_cleanup"));
    let (_dir, blocked, config) = fixture();
    let uploads = blocked.root().join(UPLOAD_DIRECTORY);
    let hook = sync_hook::install_publication(blocked.root(), Some("lease_sync_file"));
    let changed = Arc::new(StdMutex::new(None));
    let observed = changed.clone();
    hook.on_step("begin_upload_sync", move || {
        let mut entries = fs::read_dir(&uploads).unwrap();
        let stage = entries.next().unwrap().unwrap().path();
        assert!(entries.next().is_none());
        fs::remove_file(&stage).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("retained"), b"repair").unwrap();
        *observed.lock().unwrap() = Some(stage);
    });
    assert!(matches!(blocked.begin_upload_with_lease(1, config).await,
        Err(StorageError::Driver { operation, .. }) if operation == "lease_sync_file"));
    let stage = changed.lock().unwrap().clone().unwrap();
    assert_eq!(fs::read(stage.join("retained")).unwrap(), b"repair");
    assert!(!stage.with_extension("lease").exists());
    assert!(fs::read_to_string(log.path())
        .unwrap()
        .contains("failed upload begin stage cleanup; siblings retained for repair"));
}

#[cfg(unix)]
#[tokio::test]
async fn failed_temporary_cleanup_reports_and_preserves_non_id_entry() {
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_TEMP_CLEANUP_CHILD",
        false,
        |_| {},
    ) {
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(Arc::new(log.reopen().unwrap()))
            .with_ansi(false)
            .without_time()
            .finish(),
    )
    .unwrap();
    let (_dir, store, config) = fixture();
    let id = store.begin_upload_with_lease(1, config).await.unwrap();
    let before = fs::read(paths(&store, &id).1).unwrap();
    let uploads = store.root().join(UPLOAD_DIRECTORY);
    let changed = Arc::new(StdMutex::new(None));
    let observed = changed.clone();
    let hook = sync_hook::install_publication(store.root(), None);
    hook.on_step("lease_sync_file", move || {
        let mut temps = fs::read_dir(&uploads)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with('.'));
        let temp = temps.next().unwrap();
        assert!(temps.next().is_none());
        fs::remove_file(&temp).unwrap();
        fs::create_dir(&temp).unwrap();
        fs::write(temp.join("retained"), b"repair").unwrap();
        *observed.lock().unwrap() = Some(temp);
    });
    assert!(matches!(store.renew_upload(&id).await,
        Err(StorageError::Driver { operation, .. }) if operation == "lease_replace"));
    assert_eq!(fs::read(paths(&store, &id).0).unwrap(), b"");
    assert_eq!(fs::read(paths(&store, &id).1).unwrap(), before);
    let temp = changed.lock().unwrap().clone().unwrap();
    let diagnostic = fs::read_to_string(log.path()).unwrap();
    assert!(diagnostic.contains("failed upload lease temporary cleanup"));
    assert!(diagnostic.contains(temp.file_name().unwrap().to_str().unwrap()));
    assert_eq!(store.sweep_uploads(Duration::ZERO).await.unwrap(), 0);
    assert_eq!(fs::read(temp.join("retained")).unwrap(), b"repair");
    store.abort_upload(&id).await.unwrap();
    assert!(!paths(&store, &id).0.exists() && !paths(&store, &id).1.exists());
    assert_eq!(fs::read(temp.join("retained")).unwrap(), b"repair");
}
