//! Config-driven `BlobStore` selection (ADR-111 Amendment 2).
//!
//! `khive-db` cannot parse `khive.toml` itself (it sits below `khive-runtime`
//! in the crate dependency chain), so the fs-vs-s3 selector lives here, one
//! layer up, where `KhiveConfig` is already parsed. This is the choke point
//! every boot path (single- and multi-backend) resolves the configured blob
//! store through, so the two never drift onto different construction logic.

use std::sync::Arc;

use async_trait::async_trait;
#[cfg(feature = "blob-s3")]
use khive_db::stores::blob_s3::{S3BlobStore, S3BlobStoreConfig};
use khive_db::{SqliteError, StorageBackend};
use khive_storage::{
    BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef, SqlAccess,
    StorageCapability, StorageError, StorageResult, UploadId,
};

use crate::engine_config::BlobConfig;
use crate::{KhiveConfig, RuntimeError, RuntimeResult};

/// Default process-local admission budget for resident verified blob buffers.
pub const DEFAULT_BLOB_HYDRATION_BYTES: u64 = 4 * khive_storage::MAX_BLOB_WHOLE_BYTES;

/// Runtime-owned bounded blob hydration with weighted raw-byte admission.
#[derive(Debug)]
pub struct BlobHydrator {
    store: Arc<dyn BlobStore>,
    /// The store as handed in by the caller, before any read-only wrapping.
    /// Kept so the install seam can recognize a reinstall of the same raw
    /// store by pointer identity even when `store` is a wrapper around it.
    raw_store: Arc<dyn BlobStore>,
    /// True only for hydrators built through [`Self::new_read_only`], whose
    /// mutators are refused by construction. The runtime's install seam
    /// requires this on read-only runtimes: `dyn BlobStore` carries no
    /// downcast hook, so the wrapper cannot be recognized after the fact —
    /// the guarantee has to travel with the hydrator that made it.
    read_only: bool,
    /// True only for hydrators built by
    /// [`Self::resolve_for_governing_backend`], whose mode was derived from a
    /// governing backend's own access mode rather than declared by the
    /// caller. The shared install seam accepts only governed hydrators, so a
    /// hand-paired writable hydrator cannot ride it onto a read-only
    /// runtime.
    governed: bool,
    admission: Arc<tokio::sync::Semaphore>,
    budget_bytes: u64,
}

/// Error from [`BlobHydrator::resolve_for_governing_backend`], split so boot
/// can distinguish store resolution failures (which fall back to "no blob
/// configured" when `[storage.blob]` is absent) from budget pairing failures
/// (always fatal).
#[derive(Debug)]
pub enum GovernedBlobError {
    /// The configured store could not be resolved for the governing mode.
    Resolve(SqliteError),
    /// The resolved store could not be paired with the hydration budget.
    Construct(RuntimeError),
}

impl std::fmt::Display for GovernedBlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(error) => write!(f, "blob store resolution: {error}"),
            Self::Construct(error) => write!(f, "blob hydrator construction: {error}"),
        }
    }
}

impl std::error::Error for GovernedBlobError {}

impl BlobHydrator {
    /// Pair the raw store with a budget, refusing every physical mutator:
    /// the store is wrapped so `put`/`delete`/sweeps error while the bounded
    /// read surface stays available. This is the only constructor a
    /// read-only runtime's install seam accepts.
    pub(crate) fn new_read_only(
        store: Arc<dyn BlobStore>,
        budget_bytes: u64,
    ) -> RuntimeResult<Self> {
        let mut hydrator =
            Self::new_inner(wrap_read_only(Arc::clone(&store)), store, budget_bytes)?;
        hydrator.read_only = true;
        Ok(hydrator)
    }

    /// Whether this hydrator refuses mutation by construction.
    pub(crate) fn enforces_read_only(&self) -> bool {
        self.read_only
    }

    /// The store exactly as the caller handed it in, before any wrapping.
    pub(crate) fn raw_store(&self) -> Arc<dyn BlobStore> {
        Arc::clone(&self.raw_store)
    }

    /// Pair a raw store with a budget under an explicitly declared
    /// blob-runtime mode. `read_only = true` wraps the store so every
    /// physical mutator refuses while the bounded read surface stays
    /// available; `false` is [`Self::new`]. Boot paths that decide the blob
    /// mode from configuration (the blob pack's backend mode, ADR-160 D3)
    /// construct through this so the decision travels with the hydrator —
    /// the install seam can then hold hydrator mode against runtime mode
    /// instead of trusting the caller's pairing.
    pub fn for_mode(
        store: Arc<dyn BlobStore>,
        budget_bytes: u64,
        read_only: bool,
    ) -> RuntimeResult<Self> {
        if read_only {
            Self::new_read_only(store, budget_bytes)
        } else {
            Self::new(store, budget_bytes)
        }
    }

    /// Resolve the configured store and pair it with the budget under the
    /// mode of the backend that GOVERNS blob mutability — the backend the
    /// blob pack maps to (ADR-160 D3), which on single-backend boots is the
    /// runtime's own backend.
    ///
    /// The mode is derived here from `governing_backend`'s own access mode,
    /// never accepted as a caller-declared flag, and the hydrator is stamped
    /// as governed. [`crate::KhiveRuntime::install_shared_blob_hydrator`]
    /// accepts only governed hydrators.
    ///
    /// Trust model (explicit policy): which backend governs blob mutability
    /// is a deployment-topology assertion made by the host that wires boot
    /// (the blob pack's backend, ADR-160 D3), and this seam takes the
    /// caller's word for it. What the derivation defends against is the
    /// ACCIDENTAL mode mismatch — a hand-paired writable hydrator drifting
    /// onto a read-only handle through the shared seam. It does not defend
    /// against an in-process caller who deliberately misdeclares the
    /// governing backend, because no runtime seam can: any such caller can
    /// already open the configured blob root directly (the same config and
    /// store constructors are public) and mutate it without touching a
    /// runtime handle. Read-only runtime handles are a wrong-wiring guard,
    /// not an in-process sandbox.
    pub fn resolve_for_governing_backend(
        cfg: &KhiveConfig,
        resolve_backend: &StorageBackend,
        governing_backend: &StorageBackend,
        budget_bytes: u64,
    ) -> Result<Self, GovernedBlobError> {
        let read_only = governing_backend.is_read_only();
        let store = resolve_blob_store_for_mode(cfg, resolve_backend, read_only)
            .map_err(GovernedBlobError::Resolve)?;
        let mut hydrator =
            Self::for_mode(store, budget_bytes, read_only).map_err(GovernedBlobError::Construct)?;
        hydrator.governed = true;
        Ok(hydrator)
    }

    /// Whether this hydrator's mode was derived from a governing backend
    /// (see [`Self::resolve_for_governing_backend`]).
    pub(crate) fn is_governed(&self) -> bool {
        self.governed
    }

    /// Pair one store with one aggregate byte budget.
    pub fn new(store: Arc<dyn BlobStore>, budget_bytes: u64) -> RuntimeResult<Self> {
        let raw = Arc::clone(&store);
        Self::new_inner(store, raw, budget_bytes)
    }

    fn new_inner(
        store: Arc<dyn BlobStore>,
        raw_store: Arc<dyn BlobStore>,
        budget_bytes: u64,
    ) -> RuntimeResult<Self> {
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
            raw_store,
            read_only: false,
            governed: false,
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
        crate::track_named_background_task("blob_hydration", async move {
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
            s3_blob_store(bucket, region, endpoint.as_deref(), prefix.as_deref(), *allow_http)
        }
    }
}

/// Build the `[storage.blob] backend = "s3"` store from its non-secret fields
/// plus environment credentials (`S3BlobStore::new`).
#[cfg(feature = "blob-s3")]
fn s3_blob_store(
    bucket: &str,
    region: &str,
    endpoint: Option<&str>,
    prefix: Option<&str>,
    allow_http: Option<bool>,
) -> Result<Arc<dyn BlobStore>, SqliteError> {
    let mut s3_cfg = S3BlobStoreConfig::new(bucket, region);
    if let Some(endpoint) = endpoint {
        s3_cfg = s3_cfg.with_endpoint(endpoint);
    }
    if let Some(prefix) = prefix {
        s3_cfg = s3_cfg.with_prefix(prefix);
    }
    if let Some(allow_http) = allow_http {
        s3_cfg = s3_cfg.with_allow_http(allow_http);
    }
    Ok(Arc::new(S3BlobStore::new(s3_cfg)?))
}

/// A build without the `blob-s3` feature does not link the S3 store, so an S3
/// configuration is refused rather than silently served by another backend.
#[cfg(not(feature = "blob-s3"))]
fn s3_blob_store(
    _bucket: &str,
    _region: &str,
    _endpoint: Option<&str>,
    _prefix: Option<&str>,
    _allow_http: Option<bool>,
) -> Result<Arc<dyn BlobStore>, SqliteError> {
    Err(SqliteError::InvalidConfig(
        "[storage.blob] backend = \"s3\" needs a build with the `blob-s3` feature".to_string(),
    ))
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
            s3_blob_store(bucket, region, endpoint.as_deref(), prefix.as_deref(), *allow_http)?
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

    fn upload_lease_idle_cap(&self) -> Option<std::time::Duration> {
        self.inner.upload_lease_idle_cap()
    }

    async fn begin_upload_with_lease(
        &self,
        _size: u64,
        _config: khive_storage::blob::UploadLeaseConfig,
    ) -> StorageResult<UploadId> {
        Err(Self::mutation_error("begin_upload_with_lease"))
    }

    async fn renew_upload(&self, _id: &UploadId) -> StorageResult<()> {
        Err(Self::mutation_error("renew_upload"))
    }

    async fn begin_upload(&self, _declared_size: u64) -> StorageResult<UploadId> {
        Err(Self::mutation_error("begin_upload"))
    }

    async fn append_part(&self, _id: &UploadId, _bytes: Vec<u8>) -> StorageResult<u64> {
        Err(Self::mutation_error("append_part"))
    }

    async fn commit_upload(&self, _id: &UploadId, _content_ref: &ContentRef) -> StorageResult<()> {
        Err(Self::mutation_error("commit_upload"))
    }

    async fn abort_upload(&self, _id: &UploadId) -> StorageResult<()> {
        Err(Self::mutation_error("abort_upload"))
    }

    async fn sweep_uploads(&self, _idle_for: std::time::Duration) -> StorageResult<u64> {
        Err(Self::mutation_error("sweep_uploads"))
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
#[path = "blob_tests.rs"]
mod tests;
