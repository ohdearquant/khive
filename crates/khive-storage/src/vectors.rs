//! Vector embedding storage and similarity search capability.

use std::collections::HashSet;
use std::sync::OnceLock;

use async_trait::async_trait;
use uuid::Uuid;

use khive_types::SubstrateKind;

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::{
    BatchWriteSummary, IndexRebuildScope, OrphanSweepConfig, OrphanSweepResult, StorageResult,
    VectorIndexKind, VectorMetadataFilter, VectorRecord, VectorSearchHit, VectorSearchRequest,
    VectorStoreCapabilities, VectorStoreInfo,
};

/// Storage capability for dense vector embeddings and similarity search.
#[async_trait]
pub trait VectorStore: Send + Sync + 'static {
    // --- Required methods ---

    /// Store one or more dense vectors for a subject, identified by field name.
    async fn insert(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> StorageResult<()>;
    /// Insert a batch of pre-assembled vector records in one call.
    async fn insert_batch(&self, records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary>;
    /// Delete all vectors associated with the given subject ID.
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;
    /// Return the total number of vector entries in this store.
    async fn count(&self) -> StorageResult<u64>;
    /// Run approximate nearest-neighbor search and return ranked hits.
    async fn search(&self, request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>>;
    /// Return index metadata and health statistics for this backend.
    async fn info(&self) -> StorageResult<VectorStoreInfo>;
    /// Rebuild the ANN index, optionally scoped to a subset of entries.
    async fn rebuild(&self, scope: IndexRebuildScope) -> StorageResult<VectorStoreInfo>;

    // --- New methods (default impls; backends opt in by overriding) ---

    /// Declare what this backend supports (called at runtime policy construction).
    ///
    /// Default returns a conservative baseline with all optional features disabled,
    /// preserving backward compatibility for existing implementations. Backends that
    /// support filter pushdown, batch search, quantization, or in-place update should
    /// override this and return their own `&'static VectorStoreCapabilities`.
    fn capabilities(&self) -> &'static VectorStoreCapabilities {
        static BASELINE: OnceLock<VectorStoreCapabilities> = OnceLock::new();
        BASELINE.get_or_init(|| VectorStoreCapabilities {
            supports_filter: false,
            supports_batch_search: false,
            supports_quantization: false,
            supports_update: false,
            supports_orphan_sweep: false,
            supports_multi_field: false,
            // sqlite-vec 0.1.9 enforces SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192.
            // The baseline uses the same value so generic callers that have not
            // overridden capabilities() report the correct ceiling.
            max_dimensions: Some(8192),
            index_kinds: vec![VectorIndexKind::SqliteVec],
        })
    }

    /// Search with metadata pre-filter.
    ///
    /// Default: delegates to [`Self::search`] when the filter carries no predicates;
    /// returns [`StorageError::Unsupported`] otherwise. Backends with native filter
    /// pushdown should override this method and set `supports_filter = true` in their
    /// [`VectorStoreCapabilities`].
    ///
    /// Callers must check `capabilities().supports_filter` before calling; the
    /// runtime layer is responsible for post-filtering when native pushdown is absent.
    ///
    /// A backend that claims `supports_filter = true` but does not override this
    /// method will trigger a `debug_assert` at runtime.
    async fn search_with_filter(
        &self,
        request: &VectorSearchRequest,
        filter: &VectorMetadataFilter,
    ) -> StorageResult<Vec<VectorSearchHit>> {
        if filter.is_empty() {
            return self.search(request.clone()).await;
        }
        debug_assert!(
            !self.capabilities().supports_filter,
            "backend claims supports_filter=true but did not override search_with_filter"
        );
        Err(StorageError::Unsupported {
            capability: StorageCapability::Vectors,
            operation: "search_with_filter".into(),
            message: "filter pushdown not supported; set supports_filter=true only when overriding this method".into(),
        })
    }

    /// Search with N query vectors in one round-trip (HyDE fan-out, multi-query).
    ///
    /// Default: sequential calls to [`Self::search`], isolating per-query errors so one
    /// bad request does not abort the batch. Backends that support native batch
    /// search should override this and set `supports_batch_search = true`.
    async fn search_batch(
        &self,
        requests: &[VectorSearchRequest],
    ) -> StorageResult<Vec<StorageResult<Vec<VectorSearchHit>>>> {
        let mut out = Vec::with_capacity(requests.len());
        for req in requests {
            out.push(self.search(req.clone()).await);
        }
        Ok(out)
    }

    /// Re-embed an existing entry in place.
    ///
    /// Default: delete then insert. Backends that support atomic in-place update
    /// should override this and set `supports_update = true` in their
    /// [`VectorStoreCapabilities`].
    async fn update(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> StorageResult<()> {
        self.delete(subject_id).await?;
        self.insert(subject_id, kind, namespace, field, vectors)
            .await
    }

    /// Remove vectors with no live subject (orphan sweep).
    ///
    /// Default returns [`StorageError::Unsupported`]. Backends that implement
    /// deletion must set `supports_orphan_sweep = true` and override this method.
    async fn orphan_sweep(&self, config: &OrphanSweepConfig) -> StorageResult<OrphanSweepResult> {
        let _ = config;
        Err(StorageError::Unsupported {
            capability: StorageCapability::Vectors,
            operation: "orphan_sweep".into(),
            message: "this backend does not support orphan sweep".into(),
        })
    }

    /// Check which of the given subject IDs already have embeddings in this store
    /// for the specified namespace.
    ///
    /// Returns a [`HashSet`] of IDs that are present. IDs not in the returned set
    /// have no embedding. Default returns [`StorageError::Unsupported`]; backends
    /// that support fast bulk existence checks should override this method.
    async fn batch_exists(&self, ids: &[Uuid], namespace: &str) -> StorageResult<HashSet<Uuid>> {
        let _ = (ids, namespace);
        Err(StorageError::Unsupported {
            capability: StorageCapability::Vectors,
            operation: "batch_exists".into(),
            message: "this backend does not support batch existence checks".into(),
        })
    }
}
