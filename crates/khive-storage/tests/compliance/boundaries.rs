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
