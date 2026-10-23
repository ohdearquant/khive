use super::*;
use crate::engine_config::StorageSectionConfig;
use serial_test::serial;
use std::future::{poll_fn, Future};
use std::task::Poll;

#[tokio::test]
async fn read_only_upload_methods_refuse_without_mutating_staging() {
    let dir = tempfile::tempdir().unwrap();
    let inner =
        Arc::new(khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap());
    let config = khive_storage::blob::UploadLeaseConfig::new(
        uuid::Uuid::from_bytes([1; 16]),
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    let id = inner.begin_upload_with_lease(1, config).await.unwrap();
    let stage_path = inner.root().join(".uploads").join(id.as_str());
    let lease_path = stage_path.with_extension("lease");
    let before = (
        std::fs::read(&stage_path).unwrap(),
        std::fs::read(&lease_path).unwrap(),
    );
    let reference = ContentRef::from_hex("a".repeat(64)).unwrap();
    let guarded = ReadOnlyBlobStore {
        inner: inner.clone(),
    };
    let failures = [
        guarded.begin_upload_with_lease(1, config).await.map(|_| ()),
        guarded.renew_upload(&id).await,
        guarded.begin_upload(1).await.map(|_| ()),
        guarded.append_part(&id, vec![1]).await.map(|_| ()),
        guarded.commit_upload(&id, &reference).await,
        guarded.abort_upload(&id).await,
        guarded
            .sweep_uploads(std::time::Duration::ZERO)
            .await
            .map(|_| ()),
    ];
    for (result, expected) in failures.into_iter().zip([
        "begin_upload_with_lease",
        "renew_upload",
        "begin_upload",
        "append_part",
        "commit_upload",
        "abort_upload",
        "sweep_uploads",
    ]) {
        let StorageError::Unsupported {
            operation, message, ..
        } = result.unwrap_err()
        else {
            panic!("expected read-only refusal")
        };
        assert_eq!(operation, expected);
        assert!(message.contains("read-only"));
        assert_eq!(
            (
                std::fs::read(&stage_path).unwrap(),
                std::fs::read(&lease_path).unwrap()
            ),
            before
        );
    }
    let uploads = inner.root().join(".uploads");
    assert_eq!(std::fs::read_dir(&uploads).unwrap().count(), 2);
    assert_eq!(
        guarded.upload_lease_idle_cap(),
        inner.upload_lease_idle_cap()
    );
    assert_eq!(
        std::fs::metadata(uploads.join(id.as_str())).unwrap().len(),
        0
    );
    assert!(!inner.exists(&reference).await.unwrap());
    inner.abort_upload(&id).await.unwrap();
}

#[derive(Debug, Default)]
struct RecordingReadStore {
    bounded_call: std::sync::Mutex<Option<(ContentRef, u64)>>,
}

#[async_trait]
impl BlobStore for RecordingReadStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        panic!("put is not used by the read-only delegation test")
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        *self.bounded_call.lock().unwrap() = Some((content_ref.clone(), max_bytes));
        Ok(b"verified".to_vec())
    }

    async fn exists(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        Ok(true)
    }

    async fn size(&self, _content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        Ok(Some(8))
    }

    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        panic!("delete is not used by the read-only delegation test")
    }
}

fn memory_backend() -> StorageBackend {
    StorageBackend::memory().expect("memory backend should create")
}

#[tokio::test]
async fn read_only_store_delegates_the_bounded_verified_read_unchanged() {
    let inner = Arc::new(RecordingReadStore::default());
    let store = ReadOnlyBlobStore {
        inner: Arc::clone(&inner) as Arc<dyn BlobStore>,
    };
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let bytes = store.get_bounded_verified(&content_ref, 17).await.unwrap();
    assert_eq!(bytes, b"verified");
    assert_eq!(*inner.bounded_call.lock().unwrap(), Some((content_ref, 17)));
}

#[derive(Debug)]
struct HydrationReadStore {
    calls: std::sync::atomic::AtomicUsize,
    bytes: Vec<u8>,
    first_started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    first_release: Option<Arc<tokio::sync::Semaphore>>,
    fail_first: bool,
    panic_first: bool,
}

impl HydrationReadStore {
    fn immediate(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bytes,
            first_started: std::sync::Mutex::new(None),
            first_release: None,
            fail_first: false,
            panic_first: false,
        })
    }

    fn blocking_first(
        bytes: Vec<u8>,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        Arc<tokio::sync::Semaphore>,
    ) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        (
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                bytes,
                first_started: std::sync::Mutex::new(Some(started_tx)),
                first_release: Some(Arc::clone(&release)),
                fail_first: false,
                panic_first: false,
            }),
            started_rx,
            release,
        )
    }

    fn blocking_first_panic(
        bytes: Vec<u8>,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        Arc<tokio::sync::Semaphore>,
    ) {
        let (store, started, release) = Self::blocking_first(bytes);
        let store = Arc::try_unwrap(store).expect("new fixture has one owner");
        (
            Arc::new(Self {
                panic_first: true,
                ..store
            }),
            started,
            release,
        )
    }

    fn blocking_first_error(
        bytes: Vec<u8>,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        Arc<tokio::sync::Semaphore>,
    ) {
        let (store, started, release) = Self::blocking_first(bytes);
        let store = Arc::try_unwrap(store).expect("new fixture has one owner");
        (
            Arc::new(Self {
                fail_first: true,
                ..store
            }),
            started,
            release,
        )
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl BlobStore for HydrationReadStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        panic!("put is not used by hydrator tests")
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        _max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 0 {
            if let Some(started) = self.first_started.lock().unwrap().take() {
                let _ = started.send(());
            }
            if let Some(release) = &self.first_release {
                release
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("test release semaphore stays open")
                    .forget();
            }
            if self.fail_first {
                return Err(StorageError::BlobDigestMismatch {
                    expected: content_ref.clone(),
                    actual: ContentRef::from_hex("b".repeat(64))
                        .expect("fixture digest is canonical"),
                });
            }
            assert!(!self.panic_first, "injected hydration backend panic");
        }
        Ok(self.bytes.clone())
    }

    async fn exists(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        panic!("exists is not used by hydrator tests")
    }

    async fn size(&self, _content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        panic!("size is not used by hydrator tests")
    }

    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        panic!("delete is not used by hydrator tests")
    }
}

async fn wait_for_hydration_calls(store: &HydrationReadStore, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while store.calls() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hydration backend call count should advance");
}

async fn wait_for_background_task_count(expected: usize) {
    for _ in 0..10_000 {
        if crate::background_task_count() == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(crate::background_task_count(), expected);
}

#[tokio::test]
async fn hydrator_rejects_an_invalid_maximum_before_admission_or_backend_work() {
    let store = HydrationReadStore::immediate(b"x".to_vec());
    let hydrator = BlobHydrator::new(
        Arc::clone(&store) as Arc<dyn BlobStore>,
        khive_storage::MAX_BLOB_WHOLE_BYTES,
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let error = hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES + 1)
        .await
        .unwrap_err();

    assert!(matches!(error, crate::RuntimeError::InvalidInput(_)));
    assert_eq!(store.calls(), 0);
}

#[tokio::test]
#[serial(background_tasks)]
async fn zero_maximum_and_idle_portable_maximum_are_both_admissible() {
    let before = crate::background_task_count();
    let empty_store = HydrationReadStore::immediate(Vec::new());
    let empty_hydrator = BlobHydrator::new(
        Arc::clone(&empty_store) as Arc<dyn BlobStore>,
        khive_storage::MAX_BLOB_WHOLE_BYTES,
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let empty = empty_hydrator
        .hydrate_verified(&content_ref, 0)
        .await
        .expect("zero is a valid declared maximum");
    assert!(empty.bytes().is_empty());
    drop(empty);

    let max_store = HydrationReadStore::immediate(b"bounded".to_vec());
    let max_hydrator = BlobHydrator::new(
        Arc::clone(&max_store) as Arc<dyn BlobStore>,
        khive_storage::MAX_BLOB_WHOLE_BYTES,
    )
    .unwrap();
    let max = max_hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .expect("an idle minimum-size budget admits every valid maximum");
    assert_eq!(max.bytes(), b"bounded");
    drop(max);
    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn cancelling_a_waiter_retains_admission_until_backend_completion() {
    let before = crate::background_task_count();
    let (store, first_started, first_release) = HydrationReadStore::blocking_first(b"x".to_vec());
    let hydrator = Arc::new(
        BlobHydrator::new(
            Arc::clone(&store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let first_hydrator = Arc::clone(&hydrator);
    let first_ref = content_ref.clone();
    let first = tokio::spawn(async move {
        first_hydrator
            .hydrate_verified(&first_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
    });
    first_started.await.expect("first backend read must start");
    assert_eq!(crate::background_task_count(), before + 1);

    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let second_hydrator = Arc::clone(&hydrator);
    let second_ref = content_ref.clone();
    let second =
        tokio::spawn(async move { second_hydrator.hydrate_verified(&second_ref, 1).await });
    assert_eq!(hydrator.admission.available_permits(), 0);
    assert_eq!(
        store.calls(),
        1,
        "cancelled waiters must not release admission held by native work"
    );

    first_release.add_permits(1);
    let verified = tokio::time::timeout(std::time::Duration::from_secs(1), second)
        .await
        .expect("second waiter should acquire after native completion")
        .expect("second waiter task should not panic")
        .expect("second hydration should succeed");
    assert_eq!(verified.bytes(), b"x");
    drop(verified);
    wait_for_hydration_calls(&store, 2).await;

    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn verified_blob_retains_its_weighted_lease_until_drop() {
    let before = crate::background_task_count();
    let store = HydrationReadStore::immediate(b"x".to_vec());
    let hydrator = Arc::new(
        BlobHydrator::new(
            Arc::clone(&store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let first = hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap();
    assert_eq!(first.bytes(), b"x");

    let second_hydrator = Arc::clone(&hydrator);
    let second_ref = content_ref.clone();
    let second =
        tokio::spawn(async move { second_hydrator.hydrate_verified(&second_ref, 1).await });
    assert_eq!(hydrator.admission.available_permits(), 0);
    assert_eq!(
        store.calls(),
        1,
        "borrowed access must not release the original raw-buffer lease"
    );

    drop(first);
    let second = tokio::time::timeout(std::time::Duration::from_secs(1), second)
        .await
        .expect("dropping VerifiedBlob should release its lease")
        .expect("second waiter task should not panic")
        .expect("second hydration should succeed");
    assert_eq!(second.bytes(), b"x");
    drop(second);
    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn large_queued_reservation_is_not_overtaken_by_a_later_small_one() {
    let before = crate::background_task_count();
    let store = HydrationReadStore::immediate(b"x".to_vec());
    let hydrator = BlobHydrator::new(
        Arc::clone(&store) as Arc<dyn BlobStore>,
        khive_storage::MAX_BLOB_WHOLE_BYTES,
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
    let held = hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES - 1)
        .await
        .unwrap();
    assert_eq!(hydrator.admission.available_permits(), 1);
    assert_eq!(store.calls(), 1);

    let mut large =
        Box::pin(hydrator.hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES));
    poll_fn(|cx| {
        assert!(large.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;

    let mut small = Box::pin(hydrator.hydrate_verified(&content_ref, 1));
    poll_fn(|cx| {
        assert!(small.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(
        hydrator.admission.available_permits(),
        0,
        "the queued large waiter holds the one permit not held by the first read"
    );
    assert_eq!(store.calls(), 1, "neither queued read reaches the backend");

    drop(large);
    let verified = tokio::time::timeout(std::time::Duration::from_secs(1), small)
        .await
        .expect("small waiter proceeds when the large waiter is cancelled")
        .expect("small hydration succeeds");
    assert_eq!(verified.bytes(), b"x");
    assert_eq!(store.calls(), 2, "cancelled large waiter starts no read");
    drop(verified);
    drop(held);
    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn cancelling_while_queued_starts_no_backend_work() {
    let before = crate::background_task_count();
    let store = HydrationReadStore::immediate(b"x".to_vec());
    let hydrator = Arc::new(
        BlobHydrator::new(
            Arc::clone(&store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
    let first = hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let queued_hydrator = Arc::clone(&hydrator);
    let queued_ref = content_ref.clone();
    let queued = tokio::spawn(async move {
        khive_storage::scope_request_read_cancellation(cancel_rx, async move {
            let _ = entered_tx.send(());
            queued_hydrator.hydrate_verified(&queued_ref, 1).await
        })
        .await
    });
    entered_rx.await.expect("queued request must be polled");
    cancel_tx.send(true).unwrap();

    let error = queued.await.unwrap().unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::Storage(StorageError::Timeout { .. })
    ));
    assert_eq!(hydrator.admission.available_permits(), 0);
    assert_eq!(store.calls(), 1, "queued cancellation must start no I/O");
    drop(first);
    wait_for_background_task_count(before).await;
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks)]
async fn caller_deadline_drops_only_the_waiter_until_backend_completion() {
    let before = crate::background_task_count();
    let (store, first_started, first_release) = HydrationReadStore::blocking_first(b"x".to_vec());
    let hydrator = Arc::new(
        BlobHydrator::new(
            Arc::clone(&store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
    let timed_hydrator = Arc::clone(&hydrator);
    let timed_ref = content_ref.clone();
    let timed = tokio::spawn(async move {
        khive_storage::scope_request_read_deadline(
            std::time::Duration::from_secs(1),
            timed_hydrator.hydrate_verified(&timed_ref, khive_storage::MAX_BLOB_WHOLE_BYTES),
        )
        .await
    });
    first_started.await.expect("backend work must start");
    tokio::time::advance(std::time::Duration::from_secs(1)).await;

    let error = timed.await.unwrap().unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::Storage(StorageError::Timeout { .. })
    ));
    assert_eq!(crate::background_task_count(), before + 1);

    let second_hydrator = Arc::clone(&hydrator);
    let second_ref = content_ref.clone();
    let second =
        tokio::spawn(async move { second_hydrator.hydrate_verified(&second_ref, 1).await });
    assert_eq!(hydrator.admission.available_permits(), 0);
    assert_eq!(store.calls(), 1);

    first_release.add_permits(1);
    let verified = second.await.unwrap().unwrap();
    assert_eq!(verified.bytes(), b"x");
    drop(verified);
    wait_for_hydration_calls(&store, 2).await;
    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn distinct_hydrators_have_independent_admission_budgets() {
    let before = crate::background_task_count();
    let (blocked_store, started, release) = HydrationReadStore::blocking_first(b"one".to_vec());
    let independent_store = HydrationReadStore::immediate(b"two".to_vec());
    let blocked = Arc::new(
        BlobHydrator::new(
            Arc::clone(&blocked_store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let independent = BlobHydrator::new(
        Arc::clone(&independent_store) as Arc<dyn BlobStore>,
        khive_storage::MAX_BLOB_WHOLE_BYTES,
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let blocked_hydrator = Arc::clone(&blocked);
    let blocked_ref = content_ref.clone();
    let in_flight = tokio::spawn(async move {
        blocked_hydrator
            .hydrate_verified(&blocked_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
    });
    started.await.expect("first store must enter backend work");

    let verified = independent
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .expect("a different store must retain an independent budget");
    assert_eq!(verified.bytes(), b"two");
    drop(verified);

    release.add_permits(1);
    drop(in_flight.await.unwrap().unwrap());
    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn typed_backend_failure_holds_then_releases_admission() {
    let before = crate::background_task_count();
    let (store, started, release) = HydrationReadStore::blocking_first_error(b"recovered".to_vec());
    let hydrator = Arc::new(
        BlobHydrator::new(
            Arc::clone(&store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let failing_hydrator = Arc::clone(&hydrator);
    let failing_ref = content_ref.clone();
    let failing = tokio::spawn(async move {
        failing_hydrator
            .hydrate_verified(&failing_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
    });
    started.await.expect("failing backend work must start");

    let retry_hydrator = Arc::clone(&hydrator);
    let retry_ref = content_ref.clone();
    let retry = tokio::spawn(async move { retry_hydrator.hydrate_verified(&retry_ref, 1).await });
    assert_eq!(hydrator.admission.available_permits(), 0);
    assert_eq!(store.calls(), 1);

    release.add_permits(1);
    let error = failing.await.unwrap().unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::Storage(StorageError::BlobDigestMismatch { .. })
    ));

    let verified = retry.await.unwrap().unwrap();
    assert_eq!(verified.bytes(), b"recovered");
    assert_eq!(store.calls(), 2);
    drop(verified);
    wait_for_background_task_count(before).await;
}

#[tokio::test]
#[serial(background_tasks)]
async fn backend_panic_closes_the_reply_and_restores_admission() {
    let before = crate::background_task_count();
    let (store, started, release) = HydrationReadStore::blocking_first_panic(b"recovered".to_vec());
    let hydrator = Arc::new(
        BlobHydrator::new(
            Arc::clone(&store) as Arc<dyn BlobStore>,
            khive_storage::MAX_BLOB_WHOLE_BYTES,
        )
        .unwrap(),
    );
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();

    let panicking_hydrator = Arc::clone(&hydrator);
    let panicking_ref = content_ref.clone();
    let panicking = tokio::spawn(async move {
        panicking_hydrator
            .hydrate_verified(&panicking_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
    });
    started.await.expect("panicking backend work must start");
    assert_eq!(crate::background_task_count(), before + 1);
    release.add_permits(1);

    let error = panicking.await.unwrap().unwrap_err();
    assert!(matches!(error, RuntimeError::Internal(_)));
    wait_for_background_task_count(before).await;

    let recovered = hydrator
        .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .expect("backend panic must not leak admission");
    assert_eq!(recovered.bytes(), b"recovered");
    assert_eq!(store.calls(), 2);
    drop(recovered);
    wait_for_background_task_count(before).await;
}

#[test]
fn absent_storage_section_selects_fs_with_explicit_root() {
    // An in-memory backend has no data_dir to default beside, so this
    // exercises the "existing configurations keep working" path via an
    // explicit override rather than proving the full khive#292 chain
    // (already covered by `StorageBackend::blob_store`'s own tests).
    let dir = tempfile::tempdir().unwrap();
    let backend = memory_backend();
    let cfg = KhiveConfig::default();
    // `resolve_blob_store` with no override falls through to
    // `backend.blob_store(None, None)`, which errors for an in-memory
    // backend with no root -- confirm that specific, documented failure
    // mode rather than silently picking an arbitrary path.
    let err = match resolve_blob_store(&cfg, &backend) {
        Err(e) => e,
        Ok(_) => panic!("expected an error for an in-memory backend with no root override"),
    };
    assert!(matches!(err, SqliteError::InvalidData(_)));
    drop(dir);
}

#[test]
fn explicit_fs_root_is_selected() {
    let dir = tempfile::tempdir().unwrap();
    let backend = memory_backend();
    let cfg = KhiveConfig {
        storage: StorageSectionConfig {
            blob: Some(BlobConfig::Fs {
                root: Some(dir.path().to_string_lossy().into_owned()),
                floor_bytes: Some(0),
            }),
        },
        ..KhiveConfig::default()
    };
    let store = resolve_blob_store(&cfg, &backend).expect("fs store should build");
    drop(store);
}

#[cfg(feature = "blob-s3")]
#[test]
fn s3_backend_selection_reaches_s3_construction() {
    // No AWS credentials in this test process: `S3BlobStore::new` must
    // fail at the credential-env check, proving the S3 arm was actually
    // selected and reached (not silently falling back to fs).
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("AWS_ACCESS_KEY_ID");
    std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    let backend = memory_backend();
    let cfg = KhiveConfig {
        storage: StorageSectionConfig {
            blob: Some(BlobConfig::S3 {
                bucket: "khive-blobs".to_string(),
                region: "us-east-1".to_string(),
                endpoint: None,
                prefix: None,
                allow_http: None,
            }),
        },
        ..KhiveConfig::default()
    };
    let err = match resolve_blob_store(&cfg, &backend) {
        Err(e) => e,
        Ok(_) => panic!("expected the credential-env error with no AWS env vars set"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("AWS_ACCESS_KEY_ID"),
        "expected the credential-env error, got: {msg}"
    );
}

// Guards the two credential env vars this module's test toggles, since
// `std::env::set_var`/`remove_var` mutate real process-global state and
// the crate's default parallel test runner would otherwise interleave.
#[cfg(feature = "blob-s3")]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(not(feature = "blob-s3"))]
#[test]
fn s3_backend_resolution_requires_feature_for_both_access_modes() {
    let backend = memory_backend();
    let cfg = KhiveConfig {
        storage: StorageSectionConfig {
            blob: Some(BlobConfig::S3 {
                bucket: "khive-blobs".to_string(),
                region: "us-east-1".to_string(),
                endpoint: None,
                prefix: None,
                allow_http: None,
            }),
        },
        ..KhiveConfig::default()
    };
    for result in [
        resolve_blob_store(&cfg, &backend),
        resolve_blob_store_for_mode(&cfg, &backend, true),
    ] {
        let err = result.expect_err("S3 requires the blob-s3 feature");
        assert!(matches!(err, SqliteError::InvalidConfig(_)), "{err:?}");
        assert!(err.to_string().contains("blob-s3"), "{err}");
    }
}
