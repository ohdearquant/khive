//! Vector embedding storage and similarity search capability (ADR-024, ADR-041).

use std::sync::OnceLock;

use async_trait::async_trait;
use uuid::Uuid;

use khive_types::SubstrateKind;

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::{
    BatchWriteSummary, IndexRebuildScope, StorageResult, VectorIndexKind, VectorMetadataFilter,
    VectorRecord, VectorSearchHit, VectorSearchRequest, VectorStoreCapabilities, VectorStoreInfo,
};

#[async_trait]
pub trait VectorStore: Send + Sync + 'static {
    // --- Existing methods (unchanged) ---

    async fn insert(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        embedding: Vec<f32>,
    ) -> StorageResult<()>;
    async fn insert_batch(&self, records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary>;
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;
    async fn count(&self) -> StorageResult<u64>;
    async fn search(&self, request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>>;
    async fn info(&self) -> StorageResult<VectorStoreInfo>;
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
            // sqlite-vec 0.1.9 enforces SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192.
            // The baseline uses the same value so generic callers that have not
            // overridden capabilities() report the correct ceiling.
            max_dimensions: Some(8192),
            index_kinds: vec![VectorIndexKind::SqliteVec],
        })
    }

    /// Search with metadata pre-filter.
    ///
    /// Default: delegates to [`search`] when the filter carries no predicates;
    /// returns [`StorageError::Unsupported`] otherwise. Backends with native filter
    /// pushdown should override this method and set `supports_filter = true` in their
    /// [`VectorStoreCapabilities`].
    ///
    /// Callers must check `capabilities().supports_filter` before calling; the
    /// runtime layer is responsible for post-filtering when native pushdown is absent.
    async fn search_with_filter(
        &self,
        request: VectorSearchRequest,
        filter: VectorMetadataFilter,
    ) -> StorageResult<Vec<VectorSearchHit>> {
        if filter.is_empty() {
            return self.search(request).await;
        }
        Err(StorageError::Unsupported {
            capability: StorageCapability::Vectors,
            operation: "search_with_filter".into(),
            message: "filter pushdown not supported by this backend".into(),
        })
    }

    /// Search with N query vectors in one round-trip (HyDE fan-out, multi-query).
    ///
    /// Default: sequential calls to [`search`]. Backends that support native batch
    /// search (amortising index-walk overhead) should override this and set
    /// `supports_batch_search = true` in their [`VectorStoreCapabilities`].
    async fn search_batch(
        &self,
        requests: Vec<VectorSearchRequest>,
    ) -> StorageResult<Vec<Vec<VectorSearchHit>>> {
        let mut out = Vec::with_capacity(requests.len());
        for req in requests {
            out.push(self.search(req).await?);
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
        embedding: Vec<f32>,
    ) -> StorageResult<()> {
        self.delete(subject_id).await?;
        self.insert(subject_id, kind, namespace, embedding).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use uuid::Uuid;

    use khive_types::SubstrateKind;

    use super::*;
    use crate::error::StorageError;
    use crate::types::{
        BatchWriteSummary, IndexRebuildScope, VectorIndexKind, VectorMetadataFilter,
        VectorSearchHit, VectorSearchRequest, VectorStoreInfo,
    };

    // -- Minimal test fake --

    struct TestVectorStore {
        /// When `true`, `delete` returns an error.
        fail_delete: AtomicBool,
        /// When `true`, `insert` returns an error.
        fail_insert: AtomicBool,
        /// Tracks whether `delete` was called (set by the last `delete` call).
        delete_called: AtomicBool,
        /// Tracks whether `insert` was called (set by the last `insert` call).
        insert_called: AtomicBool,
    }

    impl TestVectorStore {
        fn new() -> Self {
            Self {
                fail_delete: AtomicBool::new(false),
                fail_insert: AtomicBool::new(false),
                delete_called: AtomicBool::new(false),
                insert_called: AtomicBool::new(false),
            }
        }

        fn with_fail_delete() -> Self {
            let s = Self::new();
            s.fail_delete.store(true, Ordering::SeqCst);
            s
        }

        fn with_fail_insert() -> Self {
            let s = Self::new();
            s.fail_insert.store(true, Ordering::SeqCst);
            s
        }
    }

    #[async_trait]
    impl VectorStore for TestVectorStore {
        async fn insert(
            &self,
            _subject_id: Uuid,
            _kind: SubstrateKind,
            _namespace: &str,
            _embedding: Vec<f32>,
        ) -> StorageResult<()> {
            self.insert_called.store(true, Ordering::SeqCst);
            if self.fail_insert.load(Ordering::SeqCst) {
                return Err(StorageError::InvalidInput {
                    capability: StorageCapability::Vectors,
                    operation: "insert".into(),
                    message: "injected insert failure".into(),
                });
            }
            Ok(())
        }

        async fn insert_batch(
            &self,
            records: Vec<VectorRecord>,
        ) -> StorageResult<BatchWriteSummary> {
            Ok(BatchWriteSummary {
                attempted: records.len() as u64,
                affected: records.len() as u64,
                failed: 0,
                first_error: String::new(),
            })
        }

        async fn delete(&self, _subject_id: Uuid) -> StorageResult<bool> {
            self.delete_called.store(true, Ordering::SeqCst);
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(StorageError::InvalidInput {
                    capability: StorageCapability::Vectors,
                    operation: "delete".into(),
                    message: "injected delete failure".into(),
                });
            }
            Ok(true)
        }

        async fn count(&self) -> StorageResult<u64> {
            Ok(0)
        }

        async fn search(
            &self,
            _request: VectorSearchRequest,
        ) -> StorageResult<Vec<VectorSearchHit>> {
            Ok(vec![VectorSearchHit {
                subject_id: Uuid::nil(),
                score: khive_score::DeterministicScore::from_f64(0.9),
                rank: 1,
            }])
        }

        async fn info(&self) -> StorageResult<VectorStoreInfo> {
            Ok(VectorStoreInfo {
                model_name: "test".into(),
                dimensions: 4,
                index_kind: VectorIndexKind::SqliteVec,
                entry_count: 0,
                needs_rebuild: false,
                last_rebuild_at: None,
            })
        }

        async fn rebuild(&self, _scope: IndexRebuildScope) -> StorageResult<VectorStoreInfo> {
            self.info().await
        }
    }

    // -- Test cases --

    #[tokio::test]
    async fn capabilities_returns_baseline_defaults() {
        let store = TestVectorStore::new();
        let caps = store.capabilities();
        assert!(!caps.supports_filter);
        assert!(!caps.supports_batch_search);
        assert!(!caps.supports_quantization);
        assert!(!caps.supports_update);
        // Baseline reports the sqlite-vec hard limit (SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192).
        assert_eq!(caps.max_dimensions, Some(8192));
        assert_eq!(caps.index_kinds, vec![VectorIndexKind::SqliteVec]);
    }

    /// Regression: baseline max_dimensions must be 8192 (SQLITE_VEC_VEC0_MAX_DIMENSIONS),
    /// not 4096 (SQLITE_VEC_VEC0_K_MAX). Callers with 5000-dim embeddings must not be
    /// falsely told the default backend is incapable.
    #[tokio::test]
    async fn baseline_max_dimensions_is_sqlite_vec_hard_limit() {
        let store = TestVectorStore::new();
        let caps = store.capabilities();
        let max = caps
            .max_dimensions
            .expect("baseline must declare a finite dimension limit");
        assert!(
            max >= 8192,
            "baseline max_dimensions ({max}) must be at least 8192 — SQLITE_VEC_VEC0_MAX_DIMENSIONS"
        );
    }

    #[tokio::test]
    async fn search_with_filter_empty_filter_delegates_to_search() {
        let store = TestVectorStore::new();
        let req = VectorSearchRequest {
            query_embedding: vec![0.1, 0.2, 0.3, 0.4],
            top_k: 5,
            namespace: None,
            kind: None,
        };
        let filter = VectorMetadataFilter::default(); // all fields empty
        let result = store.search_with_filter(req, filter).await;
        assert!(result.is_ok());
        let hits = result.unwrap();
        // search() on TestVectorStore returns exactly one hit
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn search_with_filter_non_empty_filter_returns_unsupported() {
        let store = TestVectorStore::new();
        let req = VectorSearchRequest {
            query_embedding: vec![0.1, 0.2, 0.3, 0.4],
            top_k: 5,
            namespace: None,
            kind: None,
        };
        let filter = VectorMetadataFilter {
            namespaces: vec!["ns:agent".into()],
            kinds: vec![],
            properties: vec![],
        };
        let result = store.search_with_filter(req, filter).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, StorageError::Unsupported { .. }),
            "expected Unsupported, got {err:?}"
        );
    }

    #[tokio::test]
    async fn search_batch_returns_one_result_per_request() {
        let store = TestVectorStore::new();
        let requests = vec![
            VectorSearchRequest {
                query_embedding: vec![0.1, 0.2, 0.3, 0.4],
                top_k: 3,
                namespace: None,
                kind: None,
            },
            VectorSearchRequest {
                query_embedding: vec![0.5, 0.6, 0.7, 0.8],
                top_k: 3,
                namespace: None,
                kind: None,
            },
        ];
        let result = store.search_batch(requests).await;
        assert!(result.is_ok());
        let batched = result.unwrap();
        assert_eq!(batched.len(), 2, "should return one result set per request");
        for hits in &batched {
            assert_eq!(hits.len(), 1, "each result set should have one hit");
        }
    }

    #[tokio::test]
    async fn search_batch_propagates_search_error() {
        // TestVectorStore.search always succeeds; inject failure via fail_insert
        // trick — instead use a custom store that fails on search.
        struct FailingSearch;

        #[async_trait]
        impl VectorStore for FailingSearch {
            async fn insert(
                &self,
                _: Uuid,
                _: SubstrateKind,
                _: &str,
                _: Vec<f32>,
            ) -> StorageResult<()> {
                Ok(())
            }
            async fn insert_batch(&self, _: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary> {
                Ok(BatchWriteSummary::default())
            }
            async fn delete(&self, _: Uuid) -> StorageResult<bool> {
                Ok(false)
            }
            async fn count(&self) -> StorageResult<u64> {
                Ok(0)
            }
            async fn search(&self, _: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>> {
                Err(StorageError::InvalidInput {
                    capability: StorageCapability::Vectors,
                    operation: "search".into(),
                    message: "injected search failure".into(),
                })
            }
            async fn info(&self) -> StorageResult<VectorStoreInfo> {
                Ok(VectorStoreInfo {
                    model_name: "fail".into(),
                    dimensions: 4,
                    index_kind: VectorIndexKind::SqliteVec,
                    entry_count: 0,
                    needs_rebuild: false,
                    last_rebuild_at: None,
                })
            }
            async fn rebuild(&self, _: IndexRebuildScope) -> StorageResult<VectorStoreInfo> {
                self.info().await
            }
        }

        let store = FailingSearch;
        let requests = vec![VectorSearchRequest {
            query_embedding: vec![0.1],
            top_k: 1,
            namespace: None,
            kind: None,
        }];
        let result = store.search_batch(requests).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn update_calls_delete_then_insert() {
        let store = TestVectorStore::new();
        let id = Uuid::new_v4();
        let result = store
            .update(id, SubstrateKind::Entity, "ns:test", vec![0.1, 0.2])
            .await;
        assert!(result.is_ok());
        assert!(
            store.delete_called.load(Ordering::SeqCst),
            "delete must be called"
        );
        assert!(
            store.insert_called.load(Ordering::SeqCst),
            "insert must be called after delete"
        );
    }

    #[tokio::test]
    async fn update_propagates_delete_failure() {
        let store = TestVectorStore::with_fail_delete();
        let id = Uuid::new_v4();
        let result = store
            .update(id, SubstrateKind::Entity, "ns:test", vec![0.1, 0.2])
            .await;
        assert!(result.is_err());
        assert!(
            store.delete_called.load(Ordering::SeqCst),
            "delete must be attempted"
        );
        assert!(
            !store.insert_called.load(Ordering::SeqCst),
            "insert must NOT be called when delete fails"
        );
    }

    #[tokio::test]
    async fn update_propagates_insert_failure() {
        let store = TestVectorStore::with_fail_insert();
        let id = Uuid::new_v4();
        let result = store
            .update(id, SubstrateKind::Entity, "ns:test", vec![0.1, 0.2])
            .await;
        assert!(result.is_err());
        assert!(
            store.insert_called.load(Ordering::SeqCst),
            "insert must be attempted"
        );
    }
}
