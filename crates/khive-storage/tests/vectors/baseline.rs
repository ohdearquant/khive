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
    search_called: AtomicBool,
}

impl TestVectorStore {
    fn new() -> Self {
        Self {
            fail_delete: AtomicBool::new(false),
            fail_insert: AtomicBool::new(false),
            delete_called: AtomicBool::new(false),
            insert_called: AtomicBool::new(false),
            search_called: AtomicBool::new(false),
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
            ..BatchWriteSummary::default()
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
        self.search_called.store(true, Ordering::SeqCst);
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

#[tokio::test]
async fn exact_candidate_scoring_defaults_to_unsupported_without_searching() {
    let store = TestVectorStore::new();
    let error = store
        .score_candidates(
            &[0.1, 0.2, 0.3, 0.4],
            &[Uuid::nil()],
            Some(SubstrateKind::Entity),
        )
        .await
        .expect_err("exact scoring requires an implementation");
    assert!(matches!(
        error,
        StorageError::Unsupported { capability: StorageCapability::Vectors, operation, .. }
            if operation == "score_candidates"
    ));
    assert!(!store.search_called.load(Ordering::SeqCst));
}

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
