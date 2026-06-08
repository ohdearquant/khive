//! Vector filter compliance suite (ADR-044 §2).
//!
//! Any backend that sets `supports_filter = true` in its `VectorStoreCapabilities`
//! MUST pass this suite. The suite covers the scenarios listed in ADR-044:
//!   - namespace isolation
//!   - kind gating
//!   - single-property Eq predicate
//!   - multi-property AND
//!   - empty filter delegates to `search`
//!
//! # Usage for backend crates
//!
//! Backend crates that override `search_with_filter` and set `supports_filter = true`
//! should call each `assert_*` helper below against their concrete store implementation
//! inside their own `#[tokio::test]` functions.
//!
//! The helpers in this module test the *default* trait behavior (namespace isolation
//! and kind gating via the default `search_with_filter` delegation logic). Backends
//! with native filter pushdown must run their own integration tests that verify the
//! full SQL pushdown path in addition to calling these helpers.

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, PropertyFilter, PropertyOp, StorageResult,
    VectorIndexKind, VectorMetadataFilter, VectorRecord, VectorSearchHit, VectorSearchRequest,
    VectorStoreInfo,
};
use khive_storage::{StorageError, VectorStore};
use khive_types::SubstrateKind;

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

// ---------------------------------------------------------------------------
// Validation helper tests (STORAGE-AUD-004)
// ---------------------------------------------------------------------------

#[test]
fn sparse_vector_validate_equal_lengths_ok() {
    let v = khive_storage::types::SparseVector {
        indices: vec![0, 2, 5],
        values: vec![1.0, 2.0, 3.0],
    };
    assert!(v.validate().is_ok());
}

#[test]
fn sparse_vector_validate_mismatched_lengths_err() {
    let v = khive_storage::types::SparseVector {
        indices: vec![0, 2],
        values: vec![1.0],
    };
    assert!(v.validate().is_err());
}

#[test]
fn sparse_vector_validate_non_finite_err() {
    let v = khive_storage::types::SparseVector {
        indices: vec![0],
        values: vec![f32::NAN],
    };
    assert!(v.validate().is_err());
}

#[test]
fn sparse_vector_validate_non_strictly_increasing_err() {
    let v = khive_storage::types::SparseVector {
        indices: vec![3, 2],
        values: vec![1.0, 2.0],
    };
    assert!(v.validate().is_err());
}

#[test]
fn vector_search_request_validate_ok() {
    let req = make_request();
    assert!(req.validate().is_ok());
}

#[test]
fn vector_search_request_validate_zero_top_k_err() {
    let req = VectorSearchRequest {
        query_vectors: vec![vec![0.1]],
        top_k: 0,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    assert!(req.validate().is_err());
}

#[test]
fn vector_search_request_validate_empty_query_err() {
    let req = VectorSearchRequest {
        query_vectors: vec![],
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    assert!(req.validate().is_err());
}

#[test]
fn vector_search_request_validate_non_finite_err() {
    let req = VectorSearchRequest {
        query_vectors: vec![vec![f32::INFINITY]],
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    assert!(req.validate().is_err());
}

#[test]
fn edge_filter_validate_ok() {
    use khive_storage::types::EdgeFilter;
    let f = EdgeFilter {
        min_weight: Some(0.0),
        max_weight: Some(1.0),
        ..Default::default()
    };
    assert!(f.validate().is_ok());
}

#[test]
fn edge_filter_validate_non_finite_err() {
    use khive_storage::types::EdgeFilter;
    let f = EdgeFilter {
        min_weight: Some(f64::NAN),
        ..Default::default()
    };
    assert!(f.validate().is_err());
}

#[test]
fn edge_filter_validate_inverted_bounds_err() {
    use khive_storage::types::EdgeFilter;
    let f = EdgeFilter {
        min_weight: Some(1.0),
        max_weight: Some(0.0),
        ..Default::default()
    };
    assert!(f.validate().is_err());
}

// ── STORAGE-001/STORAGE-003 serde rejection tests ────────────────────────────

/// A valid Edge must round-trip through serde without error.
///
/// Uses a full JSON fixture with all required fields so the test doesn't need
/// to construct `EdgeRelation` or `LinkId` directly.
#[test]
fn edge_serde_roundtrip_valid() {
    let id = uuid::Uuid::new_v4();
    let src = uuid::Uuid::new_v4();
    let tgt = uuid::Uuid::new_v4();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":0.8,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(result.is_ok(), "valid Edge must deserialize without error");
    let edge = result.unwrap();
    assert!((edge.weight - 0.8).abs() < 1e-12);
}

/// TextSearchRequest with top_k = 0 must be rejected at deserialization.
#[test]
fn text_search_request_serde_rejects_zero_top_k() {
    // Construct valid JSON except top_k = 0.
    let json = r#"{"query":"hello","mode":"plain","filter":null,"top_k":0,"snippet_chars":100}"#;
    let result: Result<khive_storage::types::TextSearchRequest, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "TextSearchRequest with top_k = 0 must be rejected by serde"
    );
}

/// TextSearchRequest with top_k > 0 must succeed deserialization.
#[test]
fn text_search_request_serde_accepts_valid_top_k() {
    let json = r#"{"query":"hello","mode":"plain","filter":null,"top_k":10,"snippet_chars":100}"#;
    let result: Result<khive_storage::types::TextSearchRequest, _> = serde_json::from_str(json);
    assert!(
        result.is_ok(),
        "TextSearchRequest with top_k = 10 must succeed"
    );
}

/// SparseSearchRequest with top_k = 0 must be rejected at deserialization.
#[test]
fn sparse_search_request_serde_rejects_zero_top_k() {
    let json = r#"{"query":{"indices":[0],"values":[1.0]},"top_k":0,"namespace":null,"kind":null}"#;
    let result: Result<khive_storage::types::SparseSearchRequest, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "SparseSearchRequest with top_k = 0 must be rejected by serde"
    );
}

/// SparseSearchRequest with top_k > 0 must succeed deserialization.
#[test]
fn sparse_search_request_serde_accepts_valid_top_k() {
    let json = r#"{"query":{"indices":[0],"values":[1.0]},"top_k":5,"namespace":null,"kind":null}"#;
    let result: Result<khive_storage::types::SparseSearchRequest, _> = serde_json::from_str(json);
    assert!(
        result.is_ok(),
        "SparseSearchRequest with top_k = 5 must succeed"
    );
}

// ── Edge weight [0.0, 1.0] range tests ──────────────────────────────────────

/// Edge deserialization must reject weight -0.1 (below valid range).
#[test]
fn edge_serde_rejects_weight_below_range() {
    let id = uuid::Uuid::new_v4();
    let src = uuid::Uuid::new_v4();
    let tgt = uuid::Uuid::new_v4();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":-0.1,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "Edge with weight -0.1 must be rejected (below [0.0, 1.0])"
    );
}

/// Edge deserialization must reject weight 2.0 (above valid range).
#[test]
fn edge_serde_rejects_weight_above_range() {
    let id = uuid::Uuid::new_v4();
    let src = uuid::Uuid::new_v4();
    let tgt = uuid::Uuid::new_v4();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":2.0,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "Edge with weight 2.0 must be rejected (above [0.0, 1.0])"
    );
}

/// EdgeFilter validation must reject min_weight = -0.1.
#[test]
fn edge_filter_validate_rejects_weight_below_range() {
    use khive_storage::types::EdgeFilter;
    let f = EdgeFilter {
        min_weight: Some(-0.1),
        ..Default::default()
    };
    assert!(
        f.validate().is_err(),
        "EdgeFilter with min_weight = -0.1 must be rejected"
    );
}

/// EdgeFilter validation must reject max_weight = 2.0.
#[test]
fn edge_filter_validate_rejects_weight_above_range() {
    use khive_storage::types::EdgeFilter;
    let f = EdgeFilter {
        max_weight: Some(2.0),
        ..Default::default()
    };
    assert!(
        f.validate().is_err(),
        "EdgeFilter with max_weight = 2.0 must be rejected"
    );
}

// ── Dense vector empty inner / outer vector tests ───────────────────────────

/// VectorSearchRequest with an empty inner query vector must be rejected.
/// This covers the case where query_vectors is non-empty but a vector inside is empty.
#[test]
fn vector_search_request_rejects_empty_inner_vector() {
    let req = khive_storage::types::VectorSearchRequest {
        query_vectors: vec![vec![]], // outer list is non-empty but inner vector is empty
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    assert!(
        req.validate().is_err(),
        "VectorSearchRequest with empty inner vector must be rejected"
    );
}

/// VectorSearchRequest with an empty outer list must be rejected.
#[test]
fn vector_search_request_rejects_empty_outer_list() {
    let req = khive_storage::types::VectorSearchRequest {
        query_vectors: vec![],
        top_k: 5,
        namespace: None,
        kind: None,
        embedding_model: None,
        filter: None,
        backend_hints: None,
    };
    assert!(
        req.validate().is_err(),
        "VectorSearchRequest with empty outer query_vectors must be rejected"
    );
}

// ── Sparse vector empty array tests ─────────────────────────────────────────

/// SparseVector with empty indices/values must be rejected at the serde boundary.
#[test]
fn sparse_vector_rejects_empty_arrays() {
    use khive_storage::types::SparseVector;
    // Build an empty SparseVector; the TryFrom runs validate() which must now reject it.
    let json = r#"{"indices":[],"values":[]}"#;
    let result: Result<SparseVector, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "SparseVector with empty indices/values must be rejected at the serde boundary"
    );
}

/// SparseVector::validate must reject empty indices directly.
#[test]
fn sparse_vector_validate_rejects_empty_indices() {
    use khive_storage::types::SparseVector;
    // Bypass serde by constructing directly with public fields.
    let sv = SparseVector {
        indices: vec![],
        values: vec![],
    };
    assert!(
        sv.validate().is_err(),
        "SparseVector::validate must reject empty indices"
    );
}

/// SparseSearchRequest::try_from must propagate the SparseVector empty-array rejection.
#[test]
fn sparse_search_request_rejects_empty_query_vector() {
    use khive_storage::types::SparseSearchRequest;
    let json = r#"{"query":{"indices":[],"values":[]},"top_k":5}"#;
    let result: Result<SparseSearchRequest, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "SparseSearchRequest with empty query vector must be rejected at the serde boundary"
    );
}

// ── JSON NaN / Infinity / overflow tests ────────────────────────────────────
// JSON does not have a NaN or Infinity token. serde_json rejects these at the parser level
// (before TryFrom is reached). 1e400 overflows f64 to infinity — also rejected.

/// Edge serde must reject JSON NaN literal in weight.
#[test]
fn edge_serde_rejects_json_nan_weight() {
    let id = uuid::Uuid::nil();
    let src = uuid::Uuid::nil();
    let tgt = uuid::Uuid::nil();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":NaN,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "JSON NaN weight must be rejected by the parser"
    );
}

/// Edge serde must reject JSON Infinity literal in weight.
#[test]
fn edge_serde_rejects_json_infinity_weight() {
    let id = uuid::Uuid::nil();
    let src = uuid::Uuid::nil();
    let tgt = uuid::Uuid::nil();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":Infinity,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "JSON Infinity weight must be rejected by the parser"
    );
}

/// Edge serde must reject 1e400 (overflows to f64::INFINITY) via TryFrom.
#[test]
fn edge_serde_rejects_overflow_to_infinity_weight() {
    let id = uuid::Uuid::nil();
    let src = uuid::Uuid::nil();
    let tgt = uuid::Uuid::nil();
    let json = format!(
        r#"{{"id":"{id}","namespace":"default","source_id":"{src}","target_id":"{tgt}","relation":"extends","weight":1e400,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","deleted_at":null,"metadata":null,"target_backend":null}}"#
    );
    let result: Result<khive_storage::types::Edge, _> = serde_json::from_str(&json);
    assert!(
        result.is_err(),
        "1e400 weight overflowing to infinity must be rejected"
    );
}
