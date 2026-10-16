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
