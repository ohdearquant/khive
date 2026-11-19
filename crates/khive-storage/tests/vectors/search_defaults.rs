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
