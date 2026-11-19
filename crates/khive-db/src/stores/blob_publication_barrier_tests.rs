#[cfg(unix)]
#[tokio::test]
async fn publication_barriers_cover_fresh_shards_and_dedup() {
    use std::os::unix::fs::MetadataExt;

    let (_dir, store) = store(0);
    let bytes = b"publication barrier order".to_vec();
    let hook = sync_hook::install_publication(store.root(), None);
    let content_ref = store.put(bytes.clone()).await.unwrap();
    assert_eq!(
        *hook.completed.lock().unwrap(),
        [
            "put_fsync",
            "put_persist",
            "put_sync_shard",
            "put_sync_parent",
            "put_sync_root"
        ]
    );
    let path = shard_path(store.root(), &content_ref);
    let directory_identity = |operation, path: &Path| {
        let metadata = fs::metadata(path).unwrap();
        (operation, metadata.dev(), metadata.ino())
    };
    let expected_directories = [
        directory_identity("put_sync_shard", path.parent().unwrap()),
        directory_identity("put_sync_parent", path.parent().unwrap().parent().unwrap()),
        directory_identity("put_sync_root", store.root()),
    ];
    assert_eq!(*hook.directories.lock().unwrap(), expected_directories);
    let inode = fs::metadata(&path).unwrap().ino();
    let reopened = FsBlobStore::open_existing(store.root().to_path_buf(), 0).unwrap();
    let hook = sync_hook::install_publication(reopened.root(), None);
    assert_eq!(reopened.put(bytes.clone()).await.unwrap(), content_ref);
    assert_eq!(
        *hook.completed.lock().unwrap(),
        [
            "put_fsync",
            "put_sync_shard",
            "put_sync_parent",
            "put_sync_root"
        ]
    );
    assert_eq!(*hook.directories.lock().unwrap(), expected_directories);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(
        reopened
            .get_bounded_verified(&content_ref, bytes.len() as u64)
            .await
            .unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[test]
fn publication_barriers_initialize_root_but_not_read_only_open() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    fs::create_dir(&root).unwrap();
    let hook = sync_hook::install_publication(&root, None);
    fs::remove_dir(&root).unwrap();
    let store = FsBlobStore::new(root.clone(), 0).unwrap();
    assert_eq!(
        *hook.completed.lock().unwrap(),
        ["init_sync_root", "init_sync_parent"]
    );
    let root_metadata = fs::metadata(&root).unwrap();
    let parent_metadata = fs::metadata(dir.path()).unwrap();
    assert_eq!(
        *hook.directories.lock().unwrap(),
        [
            ("init_sync_root", root_metadata.dev(), root_metadata.ino()),
            (
                "init_sync_parent",
                parent_metadata.dev(),
                parent_metadata.ino()
            ),
        ]
    );

    let hook = sync_hook::install_publication(store.root(), Some("init_sync_root"));
    let reopened = FsBlobStore::open_existing(root.clone(), 0).unwrap();
    assert!(hook.completed.lock().unwrap().is_empty());
    assert!(FsBlobStore::new(root, 0).is_err());
    assert!(hook.completed.lock().unwrap().is_empty());
    assert_eq!(reopened.root(), store.root());
}

#[cfg(unix)]
#[test]
fn publication_barriers_retry_each_initialization_failure() {
    for operation in ["init_sync_root", "init_sync_parent"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        fs::create_dir(&root).unwrap();
        let hook = sync_hook::install_publication(&root, Some(operation));
        fs::remove_dir(&root).unwrap();
        assert!(FsBlobStore::new(root.clone(), 0).is_err(), "{operation}");
        assert!(root.is_dir());
        assert!(!hook.completed.lock().unwrap().contains(&operation));

        let retry = sync_hook::install_publication(&root, None);
        FsBlobStore::new(root, 0).unwrap();
        assert_eq!(
            *retry.completed.lock().unwrap(),
            ["init_sync_root", "init_sync_parent"]
        );
    }
}

#[test]
fn publication_barriers_refuse_missing_root_parent_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("missing").join("nested");
    let root = parent.join("blobs");
    let error = match FsBlobStore::new(root.clone(), 0) {
        Ok(_) => panic!("missing parent must not be created"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("blob root parent missing"),
        "{error}"
    );
    assert!(
        error.to_string().contains(&parent.display().to_string()),
        "{error}"
    );
    assert!(!dir.path().join("missing").exists());

    fs::create_dir_all(&parent).unwrap();
    FsBlobStore::new(root, 0).unwrap();
}

#[cfg(unix)]
#[test]
fn publication_barriers_fsync_propagates_kernel_errors() {
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let handle = fs::File::from(std::os::fd::OwnedFd::from(socket));
    // A trace-only no-op must not pass as a filesystem barrier. Sockets
    // cannot be synchronized with fsync, so the real kernel call refuses.
    assert!(sync_directory(&handle).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn publication_barriers_fail_before_rename_without_publishing() {
    for operation in ["put_fsync", "put_persist"] {
        let (_dir, store) = store(0);
        let bytes = b"unpublished failure".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        let hook = sync_hook::install_publication(store.root(), Some(operation));
        let error = store.put(bytes.clone()).await.unwrap_err();
        assert!(error.to_string().contains(operation), "{error}");
        let path = shard_path(store.root(), &content_ref);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
        assert!(!hook.completed.lock().unwrap().contains(&operation));
        assert_eq!(store.put(bytes).await.unwrap(), content_ref);
    }
}
