use super::*;
use khive_db::StorageBackend;
use khive_storage::types::TextDocument;
use khive_types::SubstrateKind;

/// Helper: create a memory-backed StorageBackend.
fn test_backend() -> StorageBackend {
    StorageBackend::memory().expect("memory backend")
}

// -----------------------------------------------------------------------
// StorageVectorSearch tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn vector_search_basic_roundtrip() {
    let backend = test_backend();
    let store = backend.vectors("test_vs", "test-model", 3).unwrap();

    // Insert two vectors
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    store
        .insert(
            id1,
            SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    store
        .insert(
            id2,
            SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![0.0, 1.0, 0.0]],
        )
        .await
        .unwrap();

    // Wrap in adapter and use VectorSearch trait
    let adapter = StorageVectorSearch::new(store);
    let hits = adapter.vector_search(&[1.0, 0.0, 0.0], 2).await.unwrap();

    assert_eq!(hits.len(), 2);
    // Closest to [1,0,0] should be id1
    assert_eq!(hits[0].0, id1);
    // Score should be high (cosine similarity ~1.0)
    assert!(hits[0].1.to_f64() > 0.9);
}

#[tokio::test]
async fn vector_search_respects_top_k() {
    let backend = test_backend();
    let store = backend.vectors("test_topk", "test-model", 3).unwrap();

    // Insert 5 vectors
    for _ in 0..5 {
        store
            .insert(
                Uuid::new_v4(),
                SubstrateKind::Entity,
                "local",
                "content",
                vec![vec![1.0, 0.0, 0.0]],
            )
            .await
            .unwrap();
    }

    let adapter = StorageVectorSearch::new(store);
    let hits = adapter.vector_search(&[1.0, 0.0, 0.0], 3).await.unwrap();

    assert_eq!(hits.len(), 3);
}

#[tokio::test]
async fn vector_search_empty_store() {
    let backend = test_backend();
    let store = backend.vectors("test_empty", "test-model", 3).unwrap();

    let adapter = StorageVectorSearch::new(store);
    let hits = adapter.vector_search(&[1.0, 0.0, 0.0], 5).await.unwrap();

    assert!(hits.is_empty());
}

#[tokio::test]
async fn vector_search_returns_deterministic_scores() {
    let backend = test_backend();
    let store = backend.vectors("test_det", "test-model", 3).unwrap();

    let id = Uuid::new_v4();
    store
        .insert(
            id,
            SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();

    let adapter = StorageVectorSearch::new(store);

    // Run twice -- scores must be identical (deterministic)
    let hits1 = adapter.vector_search(&[1.0, 0.0, 0.0], 1).await.unwrap();
    let hits2 = adapter.vector_search(&[1.0, 0.0, 0.0], 1).await.unwrap();

    assert_eq!(hits1[0].1, hits2[0].1);
}

// -----------------------------------------------------------------------
// StorageKeywordSearch tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn keyword_search_basic_roundtrip() {
    let backend = test_backend();
    let store = backend.text("test_ks").unwrap();

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    store
        .upsert_document(TextDocument {
            subject_id: id1,
            kind: SubstrateKind::Entity,
            record_kind: None,
            namespace: "test".to_string(),
            title: Some("Rust Programming".to_string()),
            body: "Rust is a systems programming language.".to_string(),
            tags: vec![],
            metadata: None,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    store
        .upsert_document(TextDocument {
            subject_id: id2,
            kind: SubstrateKind::Entity,
            record_kind: None,
            namespace: "test".to_string(),
            title: Some("Python Guide".to_string()),
            body: "Python is a high-level programming language.".to_string(),
            tags: vec![],
            metadata: None,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // Wrap in adapter and use KeywordSearch trait
    let adapter = StorageKeywordSearch::new(store);
    let hits = adapter.keyword_search("Rust", 10).await.unwrap();

    // Should find the Rust document
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0, id1);
    assert!(hits[0].1.to_f64() > 0.0);
}

#[tokio::test]
async fn keyword_search_respects_top_k() {
    let backend = test_backend();
    let store = backend.text("test_ks_topk").unwrap();

    // Insert 5 documents all containing "programming"
    for i in 0..5 {
        store
            .upsert_document(TextDocument {
                subject_id: Uuid::new_v4(),
                kind: SubstrateKind::Note,
                record_kind: None,
                namespace: "test".to_string(),
                title: Some(format!("Doc {}", i)),
                body: format!("Programming topic number {}.", i),
                tags: vec![],
                metadata: None,
                updated_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }

    let adapter = StorageKeywordSearch::new(store);
    let hits = adapter.keyword_search("programming", 3).await.unwrap();

    assert!(hits.len() <= 3);
}

#[tokio::test]
async fn keyword_search_empty_store() {
    let backend = test_backend();
    let store = backend.text("test_ks_empty").unwrap();

    let adapter = StorageKeywordSearch::new(store);
    let hits = adapter.keyword_search("anything", 5).await.unwrap();

    assert!(hits.is_empty());
}

#[tokio::test]
async fn keyword_search_no_match() {
    let backend = test_backend();
    let store = backend.text("test_ks_nomatch").unwrap();

    store
        .upsert_document(TextDocument {
            subject_id: Uuid::new_v4(),
            kind: SubstrateKind::Entity,
            record_kind: None,
            namespace: "test".to_string(),
            title: Some("Alpha".to_string()),
            body: "Alpha article content.".to_string(),
            tags: vec![],
            metadata: None,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let adapter = StorageKeywordSearch::new(store);
    let hits = adapter
        .keyword_search("nonexistent_xyz_term", 5)
        .await
        .unwrap();

    assert!(hits.is_empty());
}

// -----------------------------------------------------------------------
// Regression tests: error mapping + overflow guards
// -----------------------------------------------------------------------

#[test]
fn storage_err_timeout_maps_to_query_timeout() {
    use khive_storage::StorageError;
    let err = StorageError::Timeout {
        operation: std::borrow::Cow::Borrowed("search"),
    };
    let ret = storage_err_to_retrieval(err, "vector search");
    assert!(
        matches!(ret, RetrievalError::QueryTimeout { .. }),
        "storage Timeout must map to QueryTimeout (transient), got: {ret:?}"
    );
    assert!(
        ret.is_transient(),
        "QueryTimeout must be classified as transient for retry logic"
    );
}

#[test]
fn storage_err_admission_timeout_maps_to_query_timeout() {
    use khive_storage::StorageError;
    // AdmissionTimeout is retryable at the storage layer (the operation
    // never began). It must reach the transient QueryTimeout arm, not fall
    // through to the permanent Hnsw/Bm25 default, otherwise a saturated-pool
    // condition becomes a hard retrieval failure.
    let err = StorageError::AdmissionTimeout {
        operation: std::borrow::Cow::Borrowed("search"),
        timeout_ms: 30_000,
        pool_identity: None,
    };
    let ret = storage_err_to_retrieval(err, "vector search");
    assert!(
        matches!(ret, RetrievalError::QueryTimeout { .. }),
        "storage AdmissionTimeout must map to QueryTimeout (transient), got: {ret:?}"
    );
    assert!(
        ret.is_transient(),
        "AdmissionTimeout must remain transient through the retrieval adapter"
    );
}

#[test]
fn storage_err_keyword_context_maps_to_bm25() {
    use khive_storage::StorageError;
    let err = StorageError::Pool {
        operation: std::borrow::Cow::Borrowed("search"),
        message: "pool full".to_string(),
    };
    let ret = storage_err_to_retrieval(err, "keyword search operation");
    assert!(
        matches!(ret, RetrievalError::Bm25(_)),
        "keyword-context errors must map to Bm25, got: {ret:?}"
    );
}

#[test]
fn storage_err_vector_context_maps_to_hnsw() {
    use khive_storage::StorageError;
    let err = StorageError::Pool {
        operation: std::borrow::Cow::Borrowed("search"),
        message: "pool full".to_string(),
    };
    let ret = storage_err_to_retrieval(err, "vector search");
    assert!(
        matches!(ret, RetrievalError::Hnsw(_)),
        "non-keyword errors must map to Hnsw, got: {ret:?}"
    );
}

#[test]
fn checked_top_k_overflow_returns_invalid_query() {
    // usize::MAX definitely overflows u32::MAX on 64-bit platforms
    let result = checked_top_k(usize::MAX);
    assert!(
        matches!(result, Err(RetrievalError::InvalidQuery(_))),
        "top_k overflow must return InvalidQuery, got: {result:?}"
    );
}

#[test]
fn checked_top_k_valid_values_succeed() {
    assert_eq!(checked_top_k(0).unwrap(), 0u32);
    assert_eq!(checked_top_k(10).unwrap(), 10u32);
    assert_eq!(checked_top_k(u32::MAX as usize).unwrap(), u32::MAX);
}

// -----------------------------------------------------------------------
// Integration: both adapters with fusion
// -----------------------------------------------------------------------

#[tokio::test]
async fn adapters_produce_fusible_results() {
    use crate::hybrid::{fuse_search_results, HybridConfig};

    let backend = test_backend();
    let vec_store = backend.vectors("test_fuse", "test-model", 3).unwrap();
    let text_store = backend.text("test_fuse").unwrap();

    let id = Uuid::new_v4();

    // Insert into both stores
    vec_store
        .insert(
            id,
            SubstrateKind::Note,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    text_store
        .upsert_document(TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            record_kind: None,
            namespace: "test".to_string(),
            title: Some("Test".to_string()),
            body: "Test document for fusion.".to_string(),
            tags: vec![],
            metadata: None,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let vec_adapter = StorageVectorSearch::new(vec_store);
    let kw_adapter = StorageKeywordSearch::new(text_store);

    let vec_hits = vec_adapter
        .vector_search(&[1.0, 0.0, 0.0], 5)
        .await
        .unwrap();
    let kw_hits = kw_adapter.keyword_search("Test", 5).await.unwrap();

    // Both should return the same UUID
    assert!(!vec_hits.is_empty());
    assert!(!kw_hits.is_empty());
    assert_eq!(vec_hits[0].0, id);
    assert_eq!(kw_hits[0].0, id);

    // Fuse the results -- same Id type (Uuid) means fusion works
    let config = HybridConfig::new(10);
    let fused = fuse_search_results(vec![vec_hits, kw_hits], &config).unwrap();

    assert!(!fused.is_empty());
    // The single shared UUID should appear in fused results
    assert_eq!(fused[0].0, id);
}
