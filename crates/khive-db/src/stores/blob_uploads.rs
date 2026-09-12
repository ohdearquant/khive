//! Staged filesystem uploads, sharing the blob root's write ownership and
//! publication primitives. Hashing and wire-session state belong to the pack.

use super::*;

const UPLOAD_DIRECTORY: &str = ".uploads";

struct UploadContext {
    root: PathBuf,
    root_handle: Arc<fs::File>,
    floor_bytes: u64,
    #[cfg(unix)]
    publication: BlobPublication,
}

async fn run<T: Send + 'static>(
    store: &FsBlobStore,
    operation: &'static str,
    work: impl FnOnce(UploadContext) -> StorageResult<T> + Send + 'static,
) -> StorageResult<T> {
    let guard = store.write_lock.clone().lock_owned().await;
    #[cfg(test)]
    let hook = sync_hook::take(&store.root);
    let context = UploadContext {
        root: store.root.clone(),
        root_handle: Arc::clone(&store.root_handle),
        floor_bytes: store.floor_bytes,
        #[cfg(unix)]
        publication: BlobPublication {
            #[cfg(test)]
            hook: hook.as_ref().and_then(|hook| hook.publication.clone()),
        },
    };
    tokio::task::spawn_blocking(move || {
        // Cancellation of the async caller cannot release ownership while a
        // blocking append or rename is still using the filesystem.
        let result = {
            let _guard = guard;
            #[cfg(test)]
            if let Some(hook) = &hook {
                let _ = hook.reached.send(());
                let _ = hook.release.recv();
            }
            (|| {
                #[cfg(unix)]
                let _file_guard =
                    acquire_root_write_lock_anchored(&context.root, &context.root_handle)?;
                #[cfg(not(unix))]
                let _file_guard = {
                    verify_blob_root_identity(&context.root, &context.root_handle)
                        .map_err(|error| map_io_err(error, operation))?;
                    acquire_root_write_lock(&context.root)?
                };
                work(context)
            })()
        };
        #[cfg(test)]
        if let Some(hook) = &hook {
            let _ = hook.done.send(());
        }
        result
    })
    .await
    .map_err(|error| StorageError::driver(StorageCapability::Blob, operation, error))?
}

fn invalid(operation: &'static str, message: impl Into<String>) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Blob,
        operation: operation.into(),
        message: message.into(),
    }
}

fn upload_error(error: std::io::Error, id: &UploadId, operation: &'static str) -> StorageError {
    if error.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound {
            capability: StorageCapability::Blob,
            resource: "upload",
            key: id.to_string(),
        }
    } else {
        map_io_err(error, operation)
    }
}

impl UploadContext {
    #[cfg(unix)]
    fn directory(&self, create: bool) -> std::io::Result<fs::File> {
        use std::os::fd::AsRawFd;
        if create {
            open_or_create_dir_at_no_follow(self.root_handle.as_raw_fd(), UPLOAD_DIRECTORY)
        } else {
            openat_dir_no_follow(self.root_handle.as_raw_fd(), UPLOAD_DIRECTORY)
        }
    }

    #[cfg(not(unix))]
    fn directory(&self, create: bool) -> std::io::Result<PathBuf> {
        let path = self.root.join(UPLOAD_DIRECTORY);
        if create {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "upload staging directory must be a real directory",
            ));
        }
        Ok(path)
    }

    fn check_space(&self, bytes: u64) -> StorageResult<()> {
        #[cfg(unix)]
        let available = available_space_at(&self.root_handle);
        #[cfg(not(unix))]
        let available = fs4::available_space(&self.root);
        let available = available.map_err(|error| map_io_err(error, "append_part_check_space"))?;
        if crosses_floor(available, bytes, self.floor_bytes) {
            return Err(StorageError::CapacityFloor {
                capability: StorageCapability::Blob,
                volume: self.root.display().to_string(),
                available_bytes: available,
                floor_bytes: self.floor_bytes,
            });
        }
        Ok(())
    }
}

pub(super) async fn begin(store: &FsBlobStore, declared_size: u64) -> StorageResult<UploadId> {
    if declared_size > MAX_BLOB_WHOLE_BYTES {
        return Err(invalid(
            "begin_upload",
            "declared upload exceeds the 64 MiB ceiling",
        ));
    }
    run(store, "begin_upload", |context| {
        let directory = context
            .directory(true)
            .map_err(|error| map_io_err(error, "begin_upload"))?;
        loop {
            let id = UploadId::from_bytes(Uuid::new_v4().as_bytes());
            #[cfg(unix)]
            let created = {
                use std::os::fd::AsRawFd;
                create_regular_file_at_no_follow(directory.as_raw_fd(), id.as_str(), 0o600)
            };
            #[cfg(not(unix))]
            let created = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(id.as_str()));
            match created {
                Ok(file) => {
                    file.sync_all()
                        .map_err(|error| map_io_err(error, "begin_upload_sync"))?;
                    return Ok(id);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(map_io_err(error, "begin_upload")),
            }
        }
    })
    .await
}

#[cfg(not(unix))]
fn open_staging(path: &Path, append: bool) -> std::io::Result<fs::File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "upload must be a regular file",
        ));
    }
    fs::OpenOptions::new().write(true).append(append).open(path)
}

pub(super) async fn append(
    store: &FsBlobStore,
    id: UploadId,
    bytes: Vec<u8>,
) -> StorageResult<u64> {
    run(store, "append_part", move |context| {
        let directory = context
            .directory(false)
            .map_err(|error| upload_error(error, &id, "append_part"))?;
        #[cfg(unix)]
        let opened = {
            use std::os::fd::AsRawFd;
            openat_regular_file_no_follow(
                directory.as_raw_fd(),
                id.as_str(),
                libc::O_WRONLY | libc::O_APPEND,
            )
        };
        #[cfg(not(unix))]
        let opened = open_staging(&directory.join(id.as_str()), true);
        let mut file = opened.map_err(|error| upload_error(error, &id, "append_part"))?;
        let before = file
            .metadata()
            .map_err(|error| map_io_err(error, "append_part_stat"))?
            .len();
        let after = before
            .checked_add(bytes.len() as u64)
            .filter(|size| *size <= MAX_BLOB_WHOLE_BYTES)
            .ok_or_else(|| invalid("append_part", "staged upload exceeds the 64 MiB ceiling"))?;
        context.check_space(bytes.len() as u64)?;
        file.write_all(&bytes)
            .map_err(|error| map_io_err(error, "append_part_write"))?;
        file.set_modified(SystemTime::now())
            .map_err(|error| map_io_err(error, "append_part_touch"))?;
        file.sync_all()
            .map_err(|error| map_io_err(error, "append_part_sync"))?;
        if file
            .metadata()
            .map_err(|error| map_io_err(error, "append_part_stat"))?
            .len()
            != after
        {
            return Err(invalid(
                "append_part",
                "staged length changed during append",
            ));
        }
        Ok(after)
    })
    .await
}

pub(super) async fn commit(
    store: &FsBlobStore,
    id: UploadId,
    content_ref: ContentRef,
) -> StorageResult<()> {
    run(store, "commit_upload", move |context| {
        commit_staged(&context, &id, &content_ref)
    })
    .await
}

#[cfg(unix)]
fn commit_staged(
    context: &UploadContext,
    id: &UploadId,
    content_ref: &ContentRef,
) -> StorageResult<()> {
    use std::os::fd::AsRawFd;
    let uploads = context
        .directory(false)
        .map_err(|error| upload_error(error, id, "commit_upload"))?;
    let staged = openat_regular_file_no_follow(uploads.as_raw_fd(), id.as_str(), libc::O_WRONLY)
        .map_err(|error| upload_error(error, id, "commit_upload"))?;
    context
        .publication
        .step("put_fsync", || staged.sync_all())?;
    let hex = content_ref.as_str();
    let shard1 = open_or_create_dir_at_no_follow(context.root_handle.as_raw_fd(), &hex[..2])
        .map_err(|error| map_io_err(error, "commit_upload_mkdir"))?;
    let shard2 = open_or_create_dir_at_no_follow(shard1.as_raw_fd(), &hex[2..4])
        .map_err(|error| map_io_err(error, "commit_upload_mkdir"))?;
    match openat_regular_file_no_follow(shard2.as_raw_fd(), hex, libc::O_WRONLY) {
        Ok(existing) => {
            existing
                .set_modified(SystemTime::now())
                .map_err(|error| map_io_err(error, "put_touch_mtime"))?;
            existing
                .sync_all()
                .map_err(|error| map_io_err(error, "put_fsync"))?;
            context
                .publication
                .sync_directories(&context.root_handle, &shard1, &shard2)?;
            unlink_entry_at(uploads.as_raw_fd(), id.as_str())
                .map_err(|error| map_io_err(error, "commit_upload_cleanup"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            publish_blob_at(
                &context.root_handle,
                &shard1,
                &shard2,
                &uploads,
                id.as_str(),
                content_ref,
                &context.publication,
            )?;
        }
        Err(error) => return Err(map_io_err(error, "commit_upload_existing")),
    }
    context
        .publication
        .sync_directory("upload_sync_staging", &uploads)
        .map_err(|error| map_io_err(error, "upload_sync_staging"))
}

#[cfg(not(unix))]
fn commit_staged(
    context: &UploadContext,
    id: &UploadId,
    content_ref: &ContentRef,
) -> StorageResult<()> {
    let uploads = context
        .directory(false)
        .map_err(|error| upload_error(error, id, "commit_upload"))?;
    let source = uploads.join(id.as_str());
    let staged =
        open_staging(&source, false).map_err(|error| upload_error(error, id, "commit_upload"))?;
    staged
        .sync_all()
        .map_err(|error| map_io_err(error, "put_fsync"))?;
    drop(staged);
    let target = shard_path(&context.root, content_ref);
    if target.exists() {
        let existing =
            open_staging(&target, false).map_err(|error| map_io_err(error, "put_touch_open"))?;
        existing
            .set_modified(SystemTime::now())
            .map_err(|error| map_io_err(error, "put_touch_mtime"))?;
        existing
            .sync_all()
            .map_err(|error| map_io_err(error, "put_fsync"))?;
        fs::remove_file(source).map_err(|error| map_io_err(error, "commit_upload_cleanup"))
    } else {
        fs::create_dir_all(target.parent().expect("blob shard parent"))
            .map_err(|error| map_io_err(error, "commit_upload_mkdir"))?;
        publish_blob_path(&source, &target)
    }
}

pub(super) async fn abort(store: &FsBlobStore, id: UploadId) -> StorageResult<()> {
    run(store, "abort_upload", move |context| {
        let result = (|| -> std::io::Result<()> {
            let directory = context.directory(false)?;
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                unlink_entry_at(directory.as_raw_fd(), id.as_str())
            }
            #[cfg(not(unix))]
            fs::remove_file(directory.join(id.as_str()))
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(map_io_err(error, "abort_upload")),
        }
    })
    .await
}

pub(super) async fn sweep(store: &FsBlobStore, idle_for: Duration) -> StorageResult<u64> {
    run(store, "sweep_uploads", move |context| {
        let directory = match context.directory(false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(map_io_err(error, "sweep_uploads_open")),
        };
        #[cfg(unix)]
        let names = {
            use std::os::fd::AsRawFd;
            read_dir_names_no_follow(directory.as_raw_fd())
        };
        #[cfg(not(unix))]
        let names = fs::read_dir(&directory).and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<Vec<_>>>()
        });
        let now = SystemTime::now();
        let mut removed = 0;
        for name in names.map_err(|error| map_io_err(error, "sweep_uploads_list"))? {
            let Ok(id) = UploadId::from_hex(name) else {
                continue;
            };
            #[cfg(unix)]
            let opened = {
                use std::os::fd::AsRawFd;
                openat_regular_file_no_follow(directory.as_raw_fd(), id.as_str(), libc::O_RDONLY)
            };
            #[cfg(not(unix))]
            let opened = open_staging(&directory.join(id.as_str()), false);
            let file = match opened {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(map_io_err(error, "sweep_uploads_stat")),
            };
            let modified = file
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(|error| map_io_err(error, "sweep_uploads_stat"))?;
            drop(file);
            if now
                .duration_since(modified)
                .is_ok_and(|age| age >= idle_for)
            {
                #[cfg(unix)]
                let deleted = {
                    use std::os::fd::AsRawFd;
                    unlink_entry_at(directory.as_raw_fd(), id.as_str())
                };
                #[cfg(not(unix))]
                let deleted = fs::remove_file(directory.join(id.as_str()));
                match deleted {
                    Ok(()) => removed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(map_io_err(error, "sweep_uploads_delete")),
                }
            }
        }
        Ok(removed)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
        (dir, store)
    }

    fn staged(store: &FsBlobStore, id: &UploadId) -> PathBuf {
        store.root().join(UPLOAD_DIRECTORY).join(id.as_str())
    }

    async fn stage(store: &FsBlobStore, bytes: &[u8]) -> (UploadId, ContentRef) {
        let id = store.begin_upload(bytes.len() as u64).await.unwrap();
        let midpoint = bytes.len() / 2;
        assert_eq!(
            store
                .append_part(&id, bytes[..midpoint].to_vec())
                .await
                .unwrap(),
            midpoint as u64
        );
        assert_eq!(
            store
                .append_part(&id, bytes[midpoint..].to_vec())
                .await
                .unwrap(),
            bytes.len() as u64
        );
        let reference = ContentRef::from_digest_bytes(blake3::hash(bytes).as_bytes());
        (id, reference)
    }

    #[tokio::test]
    async fn upload_roundtrip_and_dedup_touch_one_object() {
        let (_dir, store) = fixture();
        let bytes = b"parts make one content-addressed object";
        let (id, reference) = stage(&store, bytes).await;
        assert!(!store.exists(&reference).await.unwrap());
        assert_eq!(fs::read(staged(&store, &id)).unwrap(), bytes);
        store.commit_upload(&id, &reference).await.unwrap();
        assert!(!staged(&store, &id).exists());
        assert_eq!(
            store
                .get_bounded_verified(&reference, bytes.len() as u64)
                .await
                .unwrap(),
            bytes
        );
        assert!(matches!(
            store.append_part(&id, vec![1]).await,
            Err(StorageError::NotFound { .. })
        ));
        assert!(matches!(
            store.commit_upload(&id, &reference).await,
            Err(StorageError::NotFound { .. })
        ));

        let target = shard_path(store.root(), &reference);
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let (again, same) = stage(&store, bytes).await;
        assert_eq!(same, reference);
        store.commit_upload(&again, &same).await.unwrap();
        assert!(!staged(&store, &again).exists());
        assert!(fs::metadata(&target).unwrap().modified().unwrap() > old);
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(store.put(bytes.to_vec()).await.unwrap(), reference);
    }

    #[tokio::test]
    async fn upload_empty_object_commits() {
        let (_dir, store) = fixture();
        let (id, reference) = stage(&store, b"").await;
        store.commit_upload(&id, &reference).await.unwrap();
        assert_eq!(store.size(&reference).await.unwrap(), Some(0));
        assert_eq!(
            store.get_bounded_verified(&reference, 0).await.unwrap(),
            b""
        );
    }

    #[tokio::test]
    async fn upload_abort_and_restart_sweep_leave_committed_and_live_files() {
        let (_dir, store) = fixture();
        let object = store.put(b"committed".to_vec()).await.unwrap();
        let live = store.begin_upload(1).await.unwrap();
        store.append_part(&live, vec![1]).await.unwrap();
        let aborted = store.begin_upload(0).await.unwrap();
        store.abort_upload(&aborted).await.unwrap();
        store.abort_upload(&aborted).await.unwrap();
        assert!(!staged(&store, &aborted).exists());

        let orphan = store.begin_upload(1).await.unwrap();
        store.append_part(&orphan, vec![2]).await.unwrap();
        fs::File::options()
            .write(true)
            .open(staged(&store, &orphan))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        let root = store.root().to_path_buf();
        drop(store);
        let reopened = FsBlobStore::open_existing(root, 0).unwrap();
        assert_eq!(
            reopened
                .sweep_uploads(Duration::from_secs(3600))
                .await
                .unwrap(),
            1
        );
        assert!(!staged(&reopened, &orphan).exists());
        assert!(staged(&reopened, &live).exists());
        assert_eq!(
            reopened.get_bounded_verified(&object, 9).await.unwrap(),
            b"committed"
        );
        assert_eq!(
            reopened
                .sweep_uploads(Duration::from_secs(3600))
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn upload_each_part_checks_floor_before_writing() {
        let (_dir, mut store) = fixture();
        let id = store.begin_upload(3).await.unwrap();
        store.append_part(&id, vec![1]).await.unwrap();
        store.floor_bytes = u64::MAX;
        assert!(matches!(
            store.append_part(&id, vec![2, 3]).await,
            Err(StorageError::CapacityFloor { .. })
        ));
        assert_eq!(fs::read(staged(&store, &id)).unwrap(), [1]);
        store.floor_bytes = 0;
        assert_eq!(store.append_part(&id, vec![2, 3]).await.unwrap(), 3);
        assert_eq!(fs::read(staged(&store, &id)).unwrap(), [1, 2, 3]);
    }

    #[tokio::test]
    async fn upload_ceiling_refuses_before_creation_or_append() {
        let (_dir, store) = fixture();
        assert!(matches!(
            store.begin_upload(MAX_BLOB_WHOLE_BYTES + 1).await,
            Err(StorageError::InvalidInput { .. })
        ));
        assert!(!store.root().join(UPLOAD_DIRECTORY).exists());
        let id = store.begin_upload(MAX_BLOB_WHOLE_BYTES).await.unwrap();
        let file = fs::File::options()
            .write(true)
            .open(staged(&store, &id))
            .unwrap();
        file.set_len(MAX_BLOB_WHOLE_BYTES).unwrap();
        drop(file);
        assert!(matches!(
            store.append_part(&id, vec![1]).await,
            Err(StorageError::InvalidInput { .. })
        ));
        assert_eq!(
            fs::metadata(staged(&store, &id)).unwrap().len(),
            MAX_BLOB_WHOLE_BYTES
        );
        store.abort_upload(&id).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_commit_uses_publication_barriers_for_fresh_and_dedup() {
        use std::os::unix::fs::MetadataExt;
        let (_dir, store) = fixture();
        for dedup in [false, true] {
            let (id, reference) = stage(&store, b"publication").await;
            let hook = sync_hook::install_publication(store.root(), None);
            store.commit_upload(&id, &reference).await.unwrap();
            let mut expected = vec!["put_fsync"];
            if !dedup {
                expected.push("put_persist");
            }
            expected.extend([
                "put_sync_shard",
                "put_sync_parent",
                "put_sync_root",
                "upload_sync_staging",
            ]);
            assert_eq!(*hook.completed.lock().unwrap(), expected);
            let target = shard_path(store.root(), &reference);
            let expected_paths = [
                target.parent().unwrap().to_path_buf(),
                target.parent().unwrap().parent().unwrap().to_path_buf(),
                store.root().to_path_buf(),
                store.root().join(UPLOAD_DIRECTORY),
            ];
            let expected_inodes: Vec<_> = expected_paths
                .iter()
                .map(|path| {
                    let meta = fs::metadata(path).unwrap();
                    (meta.dev(), meta.ino())
                })
                .collect();
            let synced_inodes: Vec<_> = hook
                .directories
                .lock()
                .unwrap()
                .iter()
                .map(|(_, device, inode)| (*device, *inode))
                .collect();
            assert_eq!(synced_inodes, expected_inodes);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_publication_failures_preserve_only_complete_objects() {
        for operation in [
            "put_fsync",
            "put_persist",
            "put_sync_shard",
            "put_sync_parent",
            "put_sync_root",
            "upload_sync_staging",
        ] {
            let (_dir, store) = fixture();
            let (id, reference) = stage(&store, b"whole object").await;
            sync_hook::install_publication(store.root(), Some(operation));
            let error = store.commit_upload(&id, &reference).await.unwrap_err();
            assert!(error.to_string().contains(operation), "{error}");
            let published = !matches!(operation, "put_fsync" | "put_persist");
            assert_eq!(store.exists(&reference).await.unwrap(), published);
            if published {
                assert_eq!(
                    store.get_bounded_verified(&reference, 12).await.unwrap(),
                    b"whole object"
                );
            }
            store.abort_upload(&id).await.unwrap();
            assert!(!staged(&store, &id).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn upload_put_and_commit_share_the_publish_routine() {
        // This is a construction assertion required alongside the executed
        // barrier tests: an independently copied rename cannot satisfy it.
        let blob = include_str!("blob.rs");
        let put = blob
            .split_once("fn put_blocking_from_root_handle(")
            .unwrap()
            .1
            .split_once("\n#[cfg(not(unix))]")
            .unwrap()
            .0;
        let upload = include_str!("blob_uploads.rs");
        let commit = upload
            .split_once("fn commit_staged(")
            .unwrap()
            .1
            .split_once("\n#[cfg(not(unix))]")
            .unwrap()
            .0;
        assert_eq!(put.matches("publish_blob_at(").count(), 1);
        assert_eq!(commit.matches("publish_blob_at(").count(), 1);
        assert_eq!(blob.matches("fn publish_blob_at(").count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_staging_symlinks_never_modify_their_targets() {
        use std::os::unix::fs::symlink;
        let (dir, store) = fixture();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let victim = outside.join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        let uploads = store.root().join(UPLOAD_DIRECTORY);
        symlink(&outside, &uploads).unwrap();
        assert!(store.begin_upload(1).await.is_err());
        fs::remove_file(&uploads).unwrap();
        let id = store.begin_upload(1).await.unwrap();
        fs::remove_file(staged(&store, &id)).unwrap();
        symlink(&victim, staged(&store, &id)).unwrap();
        assert!(store.append_part(&id, vec![1]).await.is_err());
        let reference = ContentRef::from_digest_bytes(blake3::hash(b"x").as_bytes());
        assert!(store.commit_upload(&id, &reference).await.is_err());
        assert!(store.sweep_uploads(Duration::ZERO).await.is_err());
        store.abort_upload(&id).await.unwrap();
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
        assert!(!store.exists(&reference).await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_operations_refuse_a_replaced_root() {
        let (dir, store) = fixture();
        let id = store.begin_upload(1).await.unwrap();
        let moved = dir.path().join("original");
        fs::rename(store.root(), &moved).unwrap();
        fs::create_dir(store.root()).unwrap();
        let sentinel = store.root().join("sentinel");
        fs::write(&sentinel, b"replacement").unwrap();
        let reference = ContentRef::from_hex("a".repeat(64)).unwrap();
        assert!(store.begin_upload(0).await.is_err());
        assert!(store.append_part(&id, vec![1]).await.is_err());
        assert!(store.commit_upload(&id, &reference).await.is_err());
        assert!(store.abort_upload(&id).await.is_err());
        assert!(store.sweep_uploads(Duration::ZERO).await.is_err());
        assert_eq!(
            fs::metadata(moved.join(UPLOAD_DIRECTORY).join(id.as_str()))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(fs::read(sentinel).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn upload_cancelled_append_keeps_write_ownership_until_io_finishes() {
        let (_dir, store) = fixture();
        let store = Arc::new(store);
        let id = store.begin_upload(1).await.unwrap();
        let (reached, release, done) = sync_hook::install(store.root());
        let writer = store.clone();
        let upload = id.clone();
        let task = tokio::spawn(async move { writer.append_part(&upload, vec![1]).await });
        tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(store.write_lock.try_lock().is_err());
        release.send(()).unwrap();
        tokio::task::spawn_blocking(move || done.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        assert!(store.write_lock.try_lock().is_ok());
        assert_eq!(fs::read(staged(&store, &id)).unwrap(), [1]);
        store.abort_upload(&id).await.unwrap();
    }
}
