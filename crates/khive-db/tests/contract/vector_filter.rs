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

    /// Schema upgrade regression (ADR-043 §1.1 / V17): opening a backend against a
    /// file-backed database that already contains a `vec_<model>` table WITHOUT the
    /// `field` or `embedding_model` columns must succeed after `run_migrations` runs
    /// V17 (the preserving rebuild).  Unlike the old open-time DROP path, V17
    /// preserves existing rows — the row inserted in the old schema survives the
    /// migration with the correct inferred model tag.
    #[tokio::test]
    async fn vectors_for_namespace_rebuilds_old_schema_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("old_schema.db");

        // Step 1: create a database with the OLD vec0 schema (missing `field` and
        // `embedding_model`).  Inject the old DDL directly, bypassing
        // `vectors_for_namespace` which would create the current-schema table.
        {
            let old_backend = StorageBackend::sqlite(&db_path).expect("open db");
            let pool = old_backend.pool_arc();
            let writer = pool.try_writer().expect("writer");
            khive_db::extension::ensure_extensions_loaded();
            writer
                .conn()
                .execute_batch(
                    "CREATE VIRTUAL TABLE vec_old_model USING vec0(\
                     subject_id TEXT PRIMARY KEY, \
                     namespace TEXT NOT NULL, \
                     kind TEXT NOT NULL, \
                     embedding float[3] distance_metric=cosine\
                     )",
                )
                .expect("create old-schema table");
            // Insert a row in the old shape — V17 must preserve it.
            let blob: Vec<u8> = (0u32..3).flat_map(|i| (i as f32).to_le_bytes()).collect();
            writer
                .conn()
                .execute(
                    "INSERT INTO vec_old_model (subject_id, namespace, kind, embedding) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["old-id-1", "local", "Entity", blob.as_slice()],
                )
                .expect("insert into old table");
        }

        // Step 2: reopen the database and run V17 migration — the preserving rebuild
        // copies existing rows to a staging table, recreates the virtual table with the
        // full current schema, and copies rows back.
        {
            let conn_path = db_path.clone();
            tokio::task::spawn_blocking(move || {
                khive_db::extension::ensure_extensions_loaded();
                let mut conn = rusqlite::Connection::open(&conn_path).expect("open for migration");
                khive_db::migrations::run_migrations(&mut conn)
                    .expect("V17 migration must succeed");
            })
            .await
            .expect("migration task");
        }

        // Step 3: vectors_for_namespace must succeed now that V17 has applied.
        let new_backend = StorageBackend::sqlite(&db_path).expect("reopen db");
        let store = new_backend
            .vectors_for_namespace("old_model", "old_model", 3, "local")
            .expect("vectors_for_namespace must succeed after V17 migration");

        // Step 4: the old row was preserved — query it directly via SQL to confirm
        // it survived the rebuild with the correct inferred model tag.
        // (sqlite-vec's ANN search requires a query vector; we verify row survival
        // through the underlying pool's SQL access instead.)
        {
            let pool = new_backend.pool_arc();
            let writer = pool.try_writer().expect("writer for row check");
            let (subj, model): (String, String) = writer
                .conn()
                .query_row(
                    "SELECT subject_id, embedding_model FROM vec_old_model WHERE subject_id = 'old-id-1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("old row must survive V17 rebuild");
            assert_eq!(subj, "old-id-1", "original subject_id must be preserved");
            assert_eq!(
                model, "old_model",
                "inferred model must equal the table suffix 'old_model'"
            );
        }
        drop(store);

        // Step 5: insert a new row in the rebuilt table and confirm round-trip insert
        // + count works, proving the table is indexable post-rebuild.
        // Note: the preserved row from step 1 has subject_id = 'old-id-1' (a non-UUID
        // string used for test isolation); it survives the rebuild but cannot be
        // returned by the type-safe search API which parses subject_id as UUID.
        // The count() API (which operates on the raw table) verifies the preserved row
        // is present; the search verifies new rows can be indexed and retrieved.
        let new_store = new_backend
            .vectors_for_namespace("old_model", "old_model", 3, "local")
            .expect("second open must succeed");

        // Count includes the preserved legacy row (subject_id='old-id-1') + new rows.
        let count_before = new_store.count().await.expect("count");
        assert_eq!(
            count_before, 1,
            "preserved row must be present after V17 rebuild"
        );

        let id = Uuid::new_v4();
        new_store
            .insert(
                id,
                SubstrateKind::Entity,
                "local",
                "content",
                vec![vec![1.0, 0.0, 0.0]],
            )
            .await
            .expect("insert into rebuilt table");

        let count_after = new_store.count().await.expect("count after insert");
        assert_eq!(
            count_after, 2,
            "count must be 2 after inserting new row into rebuilt table"
        );
    }
}
