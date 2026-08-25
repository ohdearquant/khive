//! Config-driven `BlobStore` selection (ADR-111 Amendment 2).
//!
//! `khive-db` cannot parse `khive.toml` itself (it sits below `khive-runtime`
//! in the crate dependency chain), so the fs-vs-s3 selector lives here, one
//! layer up, where `KhiveConfig` is already parsed. This is the choke point
//! every boot path (single- and multi-backend) resolves the configured blob
//! store through, so the two never drift onto different construction logic.

use std::sync::Arc;

use async_trait::async_trait;
use khive_db::stores::blob_s3::{S3BlobStore, S3BlobStoreConfig};
use khive_db::{SqliteError, StorageBackend};
use khive_storage::{
    BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef, SqlAccess,
    StorageCapability, StorageError, StorageResult,
};

use crate::engine_config::BlobConfig;
use crate::{KhiveConfig, RuntimeError, RuntimeResult};

/// Default process-local admission budget for resident verified blob buffers.
pub const DEFAULT_BLOB_HYDRATION_BYTES: u64 = 4 * khive_storage::MAX_BLOB_WHOLE_BYTES;

/// Runtime-owned bounded blob hydration with weighted raw-byte admission.
#[derive(Debug)]
pub struct BlobHydrator {
    store: Arc<dyn BlobStore>,
    admission: Arc<tokio::sync::Semaphore>,
    budget_bytes: u64,
}

impl BlobHydrator {
    /// Pair one store with one aggregate byte budget.
    pub fn new(store: Arc<dyn BlobStore>, budget_bytes: u64) -> RuntimeResult<Self> {
        if budget_bytes < khive_storage::MAX_BLOB_WHOLE_BYTES {
            return Err(RuntimeError::InvalidInput(format!(
                "blob hydration budget must be at least {} bytes, got {budget_bytes}",
                khive_storage::MAX_BLOB_WHOLE_BYTES
            )));
        }
        let permits = usize::try_from(budget_bytes).map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "blob hydration budget {budget_bytes} does not fit this platform"
            ))
        })?;
        if permits > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(RuntimeError::InvalidInput(format!(
                "blob hydration budget {budget_bytes} exceeds the runtime maximum {}",
                tokio::sync::Semaphore::MAX_PERMITS
            )));
        }
        Ok(Self {
            store,
            admission: Arc::new(tokio::sync::Semaphore::new(permits)),
            budget_bytes,
        })
    }

    /// Return the resolved aggregate admission budget.
    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// Clone the paired store for metadata and mutation paths.
    ///
    /// Whole-buffer production reads must use [`Self::hydrate_verified`].
    pub(crate) fn store(&self) -> Arc<dyn BlobStore> {
        Arc::clone(&self.store)
    }

    /// Hydrate one complete digest-verified object under weighted admission.
    pub async fn hydrate_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> RuntimeResult<VerifiedBlob> {
        if max_bytes > khive_storage::MAX_BLOB_WHOLE_BYTES {
            return Err(RuntimeError::InvalidInput(format!(
                "blob hydration max_bytes must not exceed {} bytes, got {max_bytes}",
                khive_storage::MAX_BLOB_WHOLE_BYTES
            )));
        }

        let acquire = Arc::clone(&self.admission).acquire_many_owned(max_bytes as u32);
        let admission =
            khive_storage::await_request_read_phase("blob_hydration_admission", acquire)
                .await?
                .map_err(|error| {
                    RuntimeError::Internal(format!("blob hydration admission closed: {error}"))
                })?;

        let (sender, receiver) = tokio::sync::oneshot::channel::<RuntimeResult<VerifiedBlob>>();
        let store = Arc::clone(&self.store);
        let content_ref = content_ref.clone();
        crate::track_background_task(async move {
            // The tracked supervisor itself owns backend work and the lease.
            // Dropping a request only drops `receiver`; the supervisor stays
            // visible to daemon drain until the backend future actually ends.
            let result = match store.get_bounded_verified(&content_ref, max_bytes).await {
                Ok(bytes) => Ok(VerifiedBlob {
                    bytes,
                    _admission: admission,
                }),
                Err(error) => Err(RuntimeError::Storage(error)),
            };
            let _ = sender.send(result);
        });

        khive_storage::await_request_read_phase("blob_hydration", receiver)
            .await?
            .map_err(|_| {
                RuntimeError::Internal(
                    "blob hydration supervisor ended without delivering a result".to_string(),
                )
            })?
    }
}

/// One verified raw blob buffer and its aggregate admission lease.
///
/// The wrapper intentionally has no `Clone` or owned-byte extraction API.
/// Borrowed bytes may still be copied under the caller's own allocation budget.
///
/// ```compile_fail
/// use khive_runtime::VerifiedBlob;
///
/// fn clone_verified(blob: &VerifiedBlob) {
///     let _ = <VerifiedBlob as Clone>::clone(blob);
/// }
/// ```
///
/// ```compile_fail
/// use khive_runtime::VerifiedBlob;
///
/// fn extract_owned(blob: VerifiedBlob) -> Vec<u8> {
///     blob.into_bytes()
/// }
/// ```
///
/// ```compile_fail
/// use khive_runtime::VerifiedBlob;
///
/// fn access_private_storage(blob: &VerifiedBlob) -> usize {
///     blob.bytes.len()
/// }
/// ```
pub struct VerifiedBlob {
    bytes: Vec<u8>,
    _admission: tokio::sync::OwnedSemaphorePermit,
}

impl VerifiedBlob {
    /// Borrow the verified bytes without releasing weighted admission.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl AsRef<[u8]> for VerifiedBlob {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

impl std::fmt::Debug for VerifiedBlob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedBlob")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Resolve the `BlobStore` this `backend` should use, per `cfg.storage.blob`.
///
/// - Absent, or `backend = "fs"`: `FsBlobStore` via `StorageBackend::blob_store`,
///   using the existing `KHIVE_BLOB_ROOT` > `root` > `<db_dir>/blobs` precedence
///   (khive#292) — unchanged from every configuration written before this
///   section existed.
/// - `backend = "s3"`: `S3BlobStore`, built from the non-secret TOML fields
///   plus environment credentials (`S3BlobStore::new`).
pub fn resolve_blob_store(
    cfg: &KhiveConfig,
    backend: &StorageBackend,
) -> Result<Arc<dyn BlobStore>, SqliteError> {
    match &cfg.storage.blob {
        None => backend.blob_store(None, None),
        Some(BlobConfig::Fs { root, floor_bytes }) => {
            let root_path = root.as_ref().map(std::path::PathBuf::from);
            backend.blob_store(root_path.as_deref(), *floor_bytes)
        }
        Some(BlobConfig::S3 {
            bucket,
            region,
            endpoint,
            prefix,
            allow_http,
        }) => {
            let mut s3_cfg = S3BlobStoreConfig::new(bucket.clone(), region.clone());
            if let Some(endpoint) = endpoint {
                s3_cfg = s3_cfg.with_endpoint(endpoint.clone());
            }
            if let Some(prefix) = prefix {
                s3_cfg = s3_cfg.with_prefix(prefix.clone());
            }
            if let Some(allow_http) = allow_http {
                s3_cfg = s3_cfg.with_allow_http(*allow_http);
            }
            let store = S3BlobStore::new(s3_cfg)?;
            Ok(Arc::new(store))
        }
    }
}

/// Resolve the configured store for one pack runtime's effective access mode.
///
/// A read-only runtime retains bounded verified reads plus `exists`/`size`
/// against an already-present fs root (or a configured S3 store), but boot
/// never creates the default fs root and the wrapper rejects every physical
/// mutator. The mode belongs to the runtime assigned to the `blob` pack; a
/// mixed topology must not infer it from the main audit backend.
pub fn resolve_blob_store_for_mode(
    cfg: &KhiveConfig,
    backend: &StorageBackend,
    read_only: bool,
) -> Result<Arc<dyn BlobStore>, SqliteError> {
    if !read_only {
        return resolve_blob_store(cfg, backend);
    }

    let inner: Arc<dyn BlobStore> = match &cfg.storage.blob {
        None => backend.blob_store_read_only(None, None)?,
        Some(BlobConfig::Fs { root, floor_bytes }) => {
            let root_path = root.as_ref().map(std::path::PathBuf::from);
            backend.blob_store_read_only(root_path.as_deref(), *floor_bytes)?
        }
        Some(BlobConfig::S3 {
            bucket,
            region,
            endpoint,
            prefix,
            allow_http,
        }) => {
            let mut s3_cfg = S3BlobStoreConfig::new(bucket.clone(), region.clone());
            if let Some(endpoint) = endpoint {
                s3_cfg = s3_cfg.with_endpoint(endpoint.clone());
            }
            if let Some(prefix) = prefix {
                s3_cfg = s3_cfg.with_prefix(prefix.clone());
            }
            if let Some(allow_http) = allow_http {
                s3_cfg = s3_cfg.with_allow_http(*allow_http);
            }
            Arc::new(S3BlobStore::new(s3_cfg)?)
        }
    };
    Ok(Arc::new(ReadOnlyBlobStore { inner }))
}

/// Wrap an arbitrary store so every physical mutator is refused while the
/// bounded read surface stays available. Used by the runtime's install seam
/// to hold the read-only invariant for stores installed after boot; wrapping
/// an already-wrapped store is harmless (reads delegate, mutators refuse at
/// the outer layer).
pub(crate) fn wrap_read_only(inner: Arc<dyn BlobStore>) -> Arc<dyn BlobStore> {
    Arc::new(ReadOnlyBlobStore { inner })
}

#[derive(Debug)]
struct ReadOnlyBlobStore {
    inner: Arc<dyn BlobStore>,
}

impl ReadOnlyBlobStore {
    fn mutation_error(operation: &'static str) -> StorageError {
        StorageError::Unsupported {
            capability: StorageCapability::Blob,
            operation: operation.into(),
            message: "blob storage is read-only for this pack runtime".to_string(),
        }
    }
}

#[async_trait]
impl BlobStore for ReadOnlyBlobStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        Err(Self::mutation_error("put"))
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.inner
            .get_bounded_verified(content_ref, max_bytes)
            .await
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        self.inner.exists(content_ref).await
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        self.inner.size(content_ref).await
    }

    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        Err(Self::mutation_error("delete"))
    }

    async fn orphan_sweep(
        &self,
        _config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> {
        Err(Self::mutation_error("orphan_sweep"))
    }

    async fn transactional_orphan_sweep(
        &self,
        _sql: &dyn SqlAccess,
        _dry_run: bool,
    ) -> StorageResult<BlobOrphanSweepResult> {
        Err(Self::mutation_error("transactional_orphan_sweep"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_config::StorageSectionConfig;
    use serial_test::serial;

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
        let (store, first_started, first_release) =
            HydrationReadStore::blocking_first(b"x".to_vec());
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
        let (store, first_started, first_release) =
            HydrationReadStore::blocking_first(b"x".to_vec());
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
        let (store, started, release) =
            HydrationReadStore::blocking_first_error(b"recovered".to_vec());
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
        let retry =
            tokio::spawn(async move { retry_hydrator.hydrate_verified(&retry_ref, 1).await });
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
        let (store, started, release) =
            HydrationReadStore::blocking_first_panic(b"recovered".to_vec());
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
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
