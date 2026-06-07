//! Contract tests for sqlite vector filter semantics (ADR-009, ADR-044).
//!
//! ADR-009 §294 requires backend contract tests under `khive-db/tests/contract/`.
//! ADR-044 §232 requires a compliance fixture covering non-empty `VectorSearchRequest.filter`
//! returning `Unsupported` on backends that do not implement pushdown.

#[cfg(feature = "vectors")]
mod vector_filter_contract {
    use khive_db::StorageBackend;
    use khive_storage::types::{VectorMetadataFilter, VectorSearchRequest};
    use khive_types::SubstrateKind;
    use uuid::Uuid;

    /// Regression (ADR-044 §4): `search()` must return `StorageError::Unsupported`
    /// when the request carries a non-empty `VectorMetadataFilter`. This guards
    /// callers from silently ignoring filter predicates on backends that do not
    /// implement pushdown.
    #[tokio::test]
    async fn search_with_non_empty_filter_returns_unsupported() {
        let backend = StorageBackend::memory().expect("in-memory backend");
        let store = backend
            .vectors("filter_test", "filter_test", 3)
            .expect("vector store");

        // Insert one record so the table is non-empty.
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
            .expect("insert");

        // A request with a non-empty filter must be rejected.
        let request = VectorSearchRequest {
            query_vectors: vec![vec![1.0, 0.0, 0.0]],
            top_k: 5,
            namespace: None,
            kind: None,
            embedding_model: None,
            filter: Some(VectorMetadataFilter {
                namespaces: vec!["local".into()],
                kinds: vec![],
                property_filters: vec![],
            }),
            backend_hints: None,
        };

        let result = store.search(request).await;
        assert!(
            result.is_err(),
            "search() with non-empty filter must return Err"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, khive_storage::error::StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {err:?}"
        );
    }

    /// Regression (ADR-044 §4): `search_with_filter()` default impl must delegate
    /// to `search()` when the filter is empty, and return `Unsupported` otherwise.
    #[tokio::test]
    async fn search_with_filter_empty_delegates_and_non_empty_rejects() {
        let backend = StorageBackend::memory().expect("in-memory backend");
        let store = backend
            .vectors("filter_delegate", "filter_delegate", 3)
            .expect("vector store");

        let id = Uuid::new_v4();
        store
            .insert(
                id,
                SubstrateKind::Entity,
                "local",
                "content",
                vec![vec![0.5, 0.5, 0.0]],
            )
            .await
            .expect("insert");

        let req = VectorSearchRequest {
            query_vectors: vec![vec![0.5, 0.5, 0.0]],
            top_k: 1,
            namespace: None,
            kind: None,
            embedding_model: None,
            filter: None,
            backend_hints: None,
        };

        // Empty filter: should delegate to search() and return results.
        let empty_filter = VectorMetadataFilter::default();
        let ok = store
            .search_with_filter(&req, &empty_filter)
            .await
            .expect("empty filter must succeed");
        assert_eq!(ok.len(), 1, "empty filter must return the inserted record");

        // Non-empty filter: must return Unsupported.
        let non_empty = VectorMetadataFilter {
            namespaces: vec!["local".into()],
            kinds: vec![],
            property_filters: vec![],
        };
        let err = store
            .search_with_filter(&req, &non_empty)
            .await
            .expect_err("non-empty filter must fail on SqliteVecStore");
        assert!(
            matches!(err, khive_storage::error::StorageError::Unsupported { .. }),
            "expected StorageError::Unsupported, got {err:?}"
        );
    }
}
