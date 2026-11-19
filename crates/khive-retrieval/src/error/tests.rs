use super::*;

#[test]
fn test_error_display() {
    let err = RetrievalError::hnsw("connection failed");
    assert_eq!(err.to_string(), "hnsw error: connection failed");
}

#[test]
fn test_dimension_mismatch() {
    let err = RetrievalError::dimension_mismatch(768, 512);
    assert_eq!(err.to_string(), "dimension mismatch: expected 768, got 512");
}

#[test]
fn test_is_retryable() {
    // Non-retryable (permanent errors)
    assert!(!RetrievalError::hnsw("fail").is_retryable());
    assert!(!RetrievalError::bm25("fail").is_retryable());
    assert!(!RetrievalError::InvalidQuery("bad".into()).is_retryable());
    assert!(!RetrievalError::dimension_mismatch(768, 512).is_retryable());
}

// RETRIEVAL-06: Comprehensive error classification tests

#[test]
fn test_error_kind_transient() {
    // EmbeddingStore and LinkStore are transient (external services)
    // Note: We can't easily construct these without the actual error types,
    // so we test via is_transient/is_permanent methods on constructable errors
}

#[test]
fn test_error_kind_permanent_all_variants() {
    // All internal errors should be permanent
    let permanent_errors: Vec<RetrievalError> = vec![
        RetrievalError::hnsw("index corrupt"),
        RetrievalError::bm25("tokenization failed"),
        RetrievalError::fusion("incompatible scores"),
        RetrievalError::graph_traversal("cycle detected"),
        RetrievalError::invalid_query("empty query"),
        RetrievalError::dimension_mismatch(768, 512),
        RetrievalError::configuration("invalid k1 value"),
        RetrievalError::index_not_initialized("HNSW index"),
        RetrievalError::rebuild_required("version mismatch"),
        RetrievalError::budget_exceeded(1000, 500, 1200),
    ];

    for err in permanent_errors {
        assert!(err.is_permanent(), "Expected permanent: {err:?}");
        assert!(!err.is_transient(), "Should not be transient: {err:?}");
        assert_eq!(
            err.kind(),
            ErrorKind::Permanent,
            "Kind mismatch for: {err:?}"
        );
    }
}

#[test]
fn test_is_transient_is_permanent_consistency() {
    // is_transient and is_permanent should be mutually exclusive and exhaustive
    let test_errors: Vec<RetrievalError> = vec![
        RetrievalError::hnsw("test"),
        RetrievalError::bm25("test"),
        RetrievalError::fusion("test"),
        RetrievalError::invalid_query("test"),
        RetrievalError::dimension_mismatch(1, 2),
        RetrievalError::configuration("test"),
        RetrievalError::budget_exceeded(100, 50, 120),
    ];

    for err in test_errors {
        let transient = err.is_transient();
        let permanent = err.is_permanent();

        // XOR: exactly one should be true
        assert!(
                transient ^ permanent,
                "Error must be exactly transient OR permanent: {err:?} (transient={transient}, permanent={permanent})"
            );

        // is_retryable should match is_transient
        assert_eq!(
            err.is_retryable(),
            err.is_transient(),
            "is_retryable should equal is_transient for: {err:?}"
        );
    }
}

#[test]
fn test_error_constructors_produce_correct_messages() {
    assert_eq!(RetrievalError::hnsw("test").to_string(), "hnsw error: test");
    assert_eq!(RetrievalError::bm25("test").to_string(), "bm25 error: test");
    assert_eq!(
        RetrievalError::fusion("test").to_string(),
        "fusion error: test"
    );
    assert_eq!(
        RetrievalError::graph_traversal("test").to_string(),
        "graph traversal error: test"
    );
    assert_eq!(
        RetrievalError::invalid_query("test").to_string(),
        "invalid query: test"
    );
    assert_eq!(
        RetrievalError::configuration("test").to_string(),
        "configuration error: test"
    );
    assert_eq!(
        RetrievalError::index_not_initialized("test").to_string(),
        "index not initialized: test"
    );
    assert_eq!(
        RetrievalError::rebuild_required("test").to_string(),
        "index rebuild required: test"
    );
    assert_eq!(
        RetrievalError::budget_exceeded(100, 50, 120).to_string(),
        "memory budget exceeded: current 100 + item 50 > limit 120"
    );
}

#[test]
fn test_error_kind_enum_debug() {
    // Verify ErrorKind is Debug-able
    assert_eq!(format!("{:?}", ErrorKind::Transient), "Transient");
    assert_eq!(format!("{:?}", ErrorKind::Permanent), "Permanent");
}

#[test]
fn test_error_kind_equality() {
    // Verify ErrorKind implements PartialEq correctly
    assert_eq!(ErrorKind::Transient, ErrorKind::Transient);
    assert_eq!(ErrorKind::Permanent, ErrorKind::Permanent);
    assert_ne!(ErrorKind::Transient, ErrorKind::Permanent);
}

#[test]
fn test_query_timeout_error() {
    let err = RetrievalError::query_timeout(5000);
    assert_eq!(err.to_string(), "query timed out after 5000ms");
    assert!(err.is_transient());
    assert!(!err.is_permanent());
    assert!(err.is_retryable());
    assert_eq!(err.kind(), ErrorKind::Transient);
}

#[test]
fn test_query_cancelled_error() {
    let err = RetrievalError::query_cancelled();
    assert_eq!(err.to_string(), "query cancelled");
    assert!(err.is_transient());
    assert!(!err.is_permanent());
    assert!(err.is_retryable());
    assert_eq!(err.kind(), ErrorKind::Transient);
}

#[test]
fn test_transient_errors_classification() {
    // All transient errors should be classified correctly
    let transient_errors: Vec<RetrievalError> = vec![
        RetrievalError::query_timeout(100),
        RetrievalError::query_cancelled(),
    ];

    for err in transient_errors {
        assert!(err.is_transient(), "Expected transient: {err:?}");
        assert!(!err.is_permanent(), "Should not be permanent: {err:?}");
        assert_eq!(
            err.kind(),
            ErrorKind::Transient,
            "Kind mismatch for: {err:?}"
        );
    }
}
