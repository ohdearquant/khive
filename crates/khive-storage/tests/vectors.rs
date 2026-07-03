//! Integration tests for VectorStore default-method behavior.
//!
//! These tests were moved from `src/vectors.rs` (inline section was 420 lines,
//! exceeding the 300-line gate in the coding standards).

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, OrphanSweepConfig, VectorIndexKind, VectorMetadataFilter,
    VectorSearchHit, VectorSearchRequest, VectorStoreInfo,
};
use khive_storage::{StorageError, VectorStore};
use khive_types::SubstrateKind;

use khive_storage::capability::StorageCapability;
use khive_storage::types::{PropertyFilter, PropertyOp, StorageResult, VectorRecord};

// ---------------------------------------------------------------------------
// Minimal test fake
// ---------------------------------------------------------------------------

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
        _field: &str,
        _vectors: Vec<Vec<f32>>,
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

    async fn insert_batch(&self, records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary> {
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

    async fn search(&self, _request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>> {
        Ok(vec![VectorSearchHit {
            subject_id: Uuid::nil(),
            score: DeterministicScore::from_f64(0.9),
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

// ---------------------------------------------------------------------------
// Test cases — capabilities
// ---------------------------------------------------------------------------

/// STORAGE-AUD-001 / #485: the backend-neutral trait default must not
/// advertise sqlite-vec-specific capabilities. A minimal `VectorStore` that
/// does not override `capabilities()` must report an unknown dimension
/// ceiling and no advertised index kind.
#[tokio::test]
async fn capabilities_default_is_neutral() {
    let store = TestVectorStore::new();
    let caps = store.capabilities();
    assert!(!caps.supports_filter);
    assert!(!caps.supports_batch_search);
    assert!(!caps.supports_quantization);
    assert!(!caps.supports_update);
    assert!(!caps.supports_orphan_sweep);
    assert_eq!(
        caps.max_dimensions, None,
        "backend-neutral default must not advertise a dimension ceiling"
    );
    assert!(
        caps.index_kinds.is_empty(),
        "backend-neutral default must not advertise an index kind"
    );
}

// ---------------------------------------------------------------------------
// Test cases — search_with_filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_with_filter_empty_filter_delegates_to_search() {
    let store = TestVectorStore::new();
    let req = VectorSearchRequest {
        query_vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    let filter = VectorMetadataFilter::default(); // all fields empty
    let result = store.search_with_filter(&req, &filter).await;
    assert!(result.is_ok());
    let hits = result.unwrap();
    // search() on TestVectorStore returns exactly one hit
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn search_with_filter_non_empty_filter_returns_unsupported() {
    let store = TestVectorStore::new();
    let req = VectorSearchRequest {
        query_vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    let filter = VectorMetadataFilter {
        namespaces: vec!["ns:agent".into()],
        kinds: vec![],
        property_filters: vec![],
    };
    let result = store.search_with_filter(&req, &filter).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, StorageError::Unsupported { .. }),
        "expected Unsupported, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test cases — search_batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_batch_returns_one_result_per_request() {
    let store = TestVectorStore::new();
    let requests = vec![
        VectorSearchRequest {
            query_vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
            top_k: 3,
            namespace: None,
            kind: None,
            embedding_model: None,
            filter: None,
            backend_hints: None,
        },
        VectorSearchRequest {
            query_vectors: vec![vec![0.5, 0.6, 0.7, 0.8]],
            top_k: 3,
            namespace: None,
            kind: None,
            embedding_model: None,
            filter: None,
            backend_hints: None,
        },
    ];
    let result = store.search_batch(&requests).await;
    assert!(result.is_ok());
    let batched = result.unwrap();
    assert_eq!(batched.len(), 2, "should return one result set per request");
    for inner in &batched {
        assert!(inner.is_ok(), "each inner result should be Ok");
        assert_eq!(
            inner.as_ref().unwrap().len(),
            1,
            "each Ok should have one hit"
        );
    }
}

#[tokio::test]
async fn search_batch_isolates_per_query_errors() {
    // A store that always fails search — the outer Ok must still be returned,
    // and the failed inner result must carry the error.
    struct FailingSearch;

    #[async_trait]
    impl VectorStore for FailingSearch {
        async fn insert(
            &self,
            _: Uuid,
            _: SubstrateKind,
            _: &str,
            _: &str,
            _: Vec<Vec<f32>>,
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
        query_vectors: vec![vec![0.1]],
        top_k: 1,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    }];
    // Outer result is Ok; the error is in the inner vec.
    let result = store.search_batch(&requests).await;
    assert!(result.is_ok(), "outer result must be Ok for batch");
    let batched = result.unwrap();
    assert_eq!(batched.len(), 1);
    assert!(batched[0].is_err(), "inner result must carry the error");
}

// ---------------------------------------------------------------------------
// Test cases — orphan_sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn orphan_sweep_default_returns_unsupported() {
    let store = TestVectorStore::new();
    let config = OrphanSweepConfig {
        subject_id_allowlist: None,
        namespaces: vec![],
        substrate_kinds: vec![],
        max_delete: 100,
        dry_run: true,
    };
    let result = store.orphan_sweep(&config).await;
    assert!(
        matches!(result, Err(StorageError::Unsupported { .. })),
        "expected Unsupported, got {result:?}"
    );
    assert!(!store.capabilities().supports_orphan_sweep);
}

// ---------------------------------------------------------------------------
// Test cases — update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_calls_delete_then_insert() {
    let store = TestVectorStore::new();
    let id = Uuid::new_v4();
    let result = store
        .update(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2]],
        )
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
        .update(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2]],
        )
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
        .update(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2]],
        )
        .await;
    assert!(result.is_err());
    assert!(
        store.insert_called.load(Ordering::SeqCst),
        "insert must be attempted"
    );
}

// ---------------------------------------------------------------------------
// Test cases — VectorMetadataFilter helpers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vector_metadata_filter_is_empty_with_property_filters() {
    let empty = VectorMetadataFilter::default();
    assert!(empty.is_empty());

    let with_ns = VectorMetadataFilter {
        namespaces: vec!["ns".into()],
        ..Default::default()
    };
    assert!(!with_ns.is_empty());

    let with_prop = VectorMetadataFilter {
        property_filters: vec![PropertyFilter {
            key: "k".into(),
            op: PropertyOp::Eq,
            value: serde_json::Value::Bool(true),
        }],
        ..Default::default()
    };
    assert!(!with_prop.is_empty());
}
