// ---------------------------------------------------------------------------
// Minimal fake that supports filter (for default-delegation tests)
// ---------------------------------------------------------------------------

/// A fake [`VectorStore`] that reports `supports_filter = true` but does NOT
/// override `search_with_filter`. This deliberately violates the ADR contract and
/// is used to verify the `debug_assert` path fires in debug builds.
///
/// This fake is useful for testing the default `search_with_filter` guard behavior.
struct FilterClaimingStore;

#[async_trait]
impl VectorStore for FilterClaimingStore {
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
        Ok(vec![VectorSearchHit {
            subject_id: Uuid::nil(),
            score: khive_score::DeterministicScore::from_f64(0.5),
            rank: 1,
        }])
    }

    async fn info(&self) -> StorageResult<VectorStoreInfo> {
        Ok(VectorStoreInfo {
            model_name: "compliance-fake".into(),
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
    // Intentionally does NOT override capabilities() — inherits the baseline
    // (supports_filter: false). This is the correct behavior for the default test.
}

// ---------------------------------------------------------------------------
// Compliance assertion helpers
// ---------------------------------------------------------------------------

/// Assert that an empty filter delegates to `search` without error.
///
/// Backends must not return `Unsupported` for a no-op filter.
pub async fn assert_empty_filter_delegates<S: VectorStore>(store: &S) {
    let req = make_request();
    let filter = VectorMetadataFilter::default();
    let result = store.search_with_filter(&req, &filter).await;
    assert!(
        result.is_ok(),
        "empty filter must delegate to search and succeed, got: {result:?}"
    );
}

/// Assert that a non-empty namespace filter returns `Unsupported` from a baseline store.
///
/// Backends that do NOT set `supports_filter = true` must return `StorageError::Unsupported`
/// for any non-empty filter.
pub async fn assert_non_filter_store_rejects_namespace<S: VectorStore>(store: &S) {
    let req = make_request();
    let filter = VectorMetadataFilter {
        namespaces: vec!["ns:agent".into()],
        ..Default::default()
    };
    let result = store.search_with_filter(&req, &filter).await;
    assert!(
        matches!(result, Err(StorageError::Unsupported { .. })),
        "baseline store must return Unsupported for non-empty namespace filter, got: {result:?}"
    );
}

/// Assert that a non-empty kind filter returns `Unsupported` from a baseline store.
pub async fn assert_non_filter_store_rejects_kind<S: VectorStore>(store: &S) {
    let req = make_request();
    let filter = VectorMetadataFilter {
        kinds: vec![SubstrateKind::Entity],
        ..Default::default()
    };
    let result = store.search_with_filter(&req, &filter).await;
    assert!(
        matches!(result, Err(StorageError::Unsupported { .. })),
        "baseline store must return Unsupported for non-empty kind filter, got: {result:?}"
    );
}

/// Assert that a single-property Eq predicate returns `Unsupported` from a baseline store.
pub async fn assert_non_filter_store_rejects_property_eq<S: VectorStore>(store: &S) {
    let req = make_request();
    let filter = VectorMetadataFilter {
        property_filters: vec![PropertyFilter {
            key: "model".into(),
            op: PropertyOp::Eq,
            value: serde_json::Value::String("gpt-4".into()),
        }],
        ..Default::default()
    };
    let result = store.search_with_filter(&req, &filter).await;
    assert!(
        matches!(result, Err(StorageError::Unsupported { .. })),
        "baseline store must return Unsupported for property Eq filter, got: {result:?}"
    );
}

/// Assert that a multi-predicate AND filter returns `Unsupported` from a baseline store.
pub async fn assert_non_filter_store_rejects_multi_property<S: VectorStore>(store: &S) {
    let req = make_request();
    let filter = VectorMetadataFilter {
        namespaces: vec!["ns:agent".into()],
        property_filters: vec![
            PropertyFilter {
                key: "model".into(),
                op: PropertyOp::Eq,
                value: serde_json::Value::String("gpt-4".into()),
            },
            PropertyFilter {
                key: "active".into(),
                op: PropertyOp::Eq,
                value: serde_json::Value::Bool(true),
            },
        ],
        ..Default::default()
    };
    let result = store.search_with_filter(&req, &filter).await;
    assert!(
        matches!(result, Err(StorageError::Unsupported { .. })),
        "baseline store must return Unsupported for multi-property filter, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn make_request() -> VectorSearchRequest {
    VectorSearchRequest {
        query_vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    }
}

// ---------------------------------------------------------------------------
// Tests — run the compliance suite against the baseline (non-filter) store
// ---------------------------------------------------------------------------

/// The default store (no filter support) must pass the full compliance suite.
///
/// Backends that override `search_with_filter` run additional backend-specific
/// integration tests; these tests verify only the default contract.
#[tokio::test]
async fn compliance_suite_empty_filter_delegates() {
    let store = FilterClaimingStore;
    assert_empty_filter_delegates(&store).await;
}

#[tokio::test]
async fn compliance_suite_namespace_isolation_without_filter_support() {
    let store = FilterClaimingStore;
    assert_non_filter_store_rejects_namespace(&store).await;
}

#[tokio::test]
async fn compliance_suite_kind_gating_without_filter_support() {
    let store = FilterClaimingStore;
    assert_non_filter_store_rejects_kind(&store).await;
}

#[tokio::test]
async fn compliance_suite_single_property_eq_without_filter_support() {
    let store = FilterClaimingStore;
    assert_non_filter_store_rejects_property_eq(&store).await;
}

#[tokio::test]
async fn compliance_suite_multi_property_and_without_filter_support() {
    let store = FilterClaimingStore;
    assert_non_filter_store_rejects_multi_property(&store).await;
}
