use std::sync::Arc;

use khive_storage::types::{SparseVector, TextDocument, TextFilter};
use khive_storage::{SparseStoreFactory, StorageCapability, StorageError, TextSearchFactory};
use khive_types::SubstrateKind;
use uuid::Uuid;

use crate::backend::StorageBackend;
use crate::error::SqliteError;

fn assert_invalid(error: StorageError, expected: StorageCapability, operation_name: &str) {
    let StorageError::Driver {
        capability,
        operation,
        source,
    } = error
    else {
        panic!("{error}")
    };
    assert_eq!(capability, expected);
    assert_eq!(operation, operation_name);
    assert!(matches!(
        source.downcast_ref::<SqliteError>(),
        Some(SqliteError::InvalidData(_))
    ));
}

#[tokio::test]
async fn sparse_factory_preserves_model_and_namespace_bindings_on_memory_backend() {
    let backend = Arc::new(StorageBackend::memory().unwrap());
    let factory: Arc<dyn SparseStoreFactory> = backend;
    let alpha = factory.open("first", "alpha").unwrap();
    let beta = factory.open("first", "beta").unwrap();
    let second = factory.open("second", "alpha").unwrap();
    let id = Uuid::new_v4();
    alpha
        .insert_sparse(
            id,
            SubstrateKind::Entity,
            "alpha",
            "content",
            SparseVector {
                indices: vec![1, 7],
                values: vec![0.5, 1.0],
            },
        )
        .await
        .unwrap();
    assert_eq!(alpha.count().await.unwrap(), 1);
    assert_eq!(beta.count().await.unwrap(), 0);
    assert_eq!(second.count().await.unwrap(), 0);
    assert_eq!(
        factory
            .open("first", "alpha")
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn text_factory_preserves_table_tokenizer_and_namespace_queries() {
    let backend = Arc::new(StorageBackend::memory().unwrap());
    let factory: Arc<dyn TextSearchFactory> = backend.clone();
    let words = factory.open("words", "unicode61").unwrap();
    let grams = factory.open("grams", "trigram").unwrap();
    let id = Uuid::new_v4();
    words
        .upsert_document(TextDocument {
            subject_id: id,
            kind: SubstrateKind::Entity,
            record_kind: Some("concept".into()),
            namespace: "alpha".into(),
            title: None,
            body: "specific evidence".into(),
            tags: vec![],
            metadata: None,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    assert_eq!(words.count(TextFilter::default()).await.unwrap(), 1);
    assert_eq!(grams.count(TextFilter::default()).await.unwrap(), 0);
    assert!(words.get_document("beta", id).await.unwrap().is_none());
    assert_eq!(
        factory
            .open("words", "unicode61")
            .unwrap()
            .get_document("alpha", id)
            .await
            .unwrap()
            .unwrap()
            .body,
        "specific evidence"
    );
    let reader = backend.pool.reader().unwrap();
    for (table, tokenizer) in [("fts_words", "unicode61"), ("fts_grams", "trigram")] {
        let sql: String = reader
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains(&format!("tokenize = '{tokenizer}'")), "{sql}");
    }
}

#[test]
fn factories_keep_validation_labels_and_original_backend_errors() {
    let backend = StorageBackend::memory().unwrap();
    for (model, namespace) in [("bad-key", "local"), ("good", " ")] {
        assert_invalid(
            SparseStoreFactory::open(&backend, model, namespace)
                .err()
                .unwrap(),
            StorageCapability::Sparse,
            "sparse_for_namespace",
        );
    }
    for (table, tokenizer) in [
        ("bad-key", "trigram"),
        ("good_rowids", "trigram"),
        ("good", "bad-tokenizer"),
    ] {
        assert_invalid(
            TextSearchFactory::open(&backend, table, tokenizer)
                .err()
                .unwrap(),
            StorageCapability::Text,
            "text_with_tokenizer",
        );
    }
    #[cfg(feature = "vectors")]
    for (model, namespace) in [("bad-key", "local"), ("good", " ")] {
        assert_invalid(
            khive_storage::VectorStoreFactory::open(&backend, model, "model-name", 3, namespace)
                .err()
                .unwrap(),
            StorageCapability::Vectors,
            "vectors_for_namespace",
        );
    }
}

#[tokio::test]
async fn readonly_factories_open_present_bindings_and_refuse_absent_without_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factories.db");
    {
        let backend = StorageBackend::sqlite_for_test(&path).unwrap();
        backend.prepare_core_schema().unwrap();
        SparseStoreFactory::open(&backend, "present", "local").unwrap();
        TextSearchFactory::open(&backend, "present", "unicode61").unwrap();
        #[cfg(feature = "vectors")]
        khive_storage::VectorStoreFactory::open(&backend, "present", "name", 3, "local").unwrap();
    }
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let before = readonly.pool.writer_acquisition_snapshot();
    assert_eq!(
        SparseStoreFactory::open(&readonly, "present", "local")
            .unwrap()
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        TextSearchFactory::open(&readonly, "present", "unicode61")
            .unwrap()
            .count(TextFilter::default())
            .await
            .unwrap(),
        0
    );
    assert_invalid(
        SparseStoreFactory::open(&readonly, "missing", "local")
            .err()
            .unwrap(),
        StorageCapability::Sparse,
        "sparse_for_namespace",
    );
    assert_invalid(
        TextSearchFactory::open(&readonly, "missing", "trigram")
            .err()
            .unwrap(),
        StorageCapability::Text,
        "text_with_tokenizer",
    );
    #[cfg(feature = "vectors")]
    {
        assert_eq!(
            khive_storage::VectorStoreFactory::open(&readonly, "present", "name", 3, "local")
                .unwrap()
                .count()
                .await
                .unwrap(),
            0
        );
        assert_invalid(
            khive_storage::VectorStoreFactory::open(&readonly, "missing", "name", 3, "local")
                .err()
                .unwrap(),
            StorageCapability::Vectors,
            "vectors_for_namespace",
        );
    }
    assert_eq!(readonly.pool.writer_acquisition_snapshot(), before);
    let reader = readonly.pool.reader().unwrap();
    assert_eq!(reader.conn().query_row("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('sparse_missing','fts_missing','vec_missing')", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[cfg(feature = "vectors")]
#[tokio::test]
async fn vector_factory_keeps_model_dimensions_and_namespace_without_default_fallback() {
    use khive_storage::VectorStoreFactory;
    let backend = Arc::new(StorageBackend::memory().unwrap());
    let factory: Arc<dyn VectorStoreFactory> = backend.clone();
    let selected = factory
        .open("selected", "provider-selected", 3, "alpha")
        .unwrap();
    let other_ns = factory
        .open("selected", "provider-selected", 3, "beta")
        .unwrap();
    let other_model = factory.open("other", "provider-other", 2, "alpha").unwrap();
    let id = Uuid::new_v4();
    selected
        .insert(
            id,
            SubstrateKind::Entity,
            "alpha",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    assert_eq!(selected.count().await.unwrap(), 1);
    assert_eq!(other_ns.count().await.unwrap(), 0);
    assert_eq!(other_model.count().await.unwrap(), 0);
    let reader = backend.pool.reader().unwrap();
    let row: (String, String, i64) = reader
        .conn()
        .query_row(
            "SELECT embedding_model, namespace, length(embedding) FROM vec_selected",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("provider-selected".into(), "alpha".into(), 12));
    drop(reader);
    assert!(other_model
        .insert(
            Uuid::new_v4(),
            SubstrateKind::Entity,
            "alpha",
            "content",
            vec![vec![1.0, 0.0, 0.0]]
        )
        .await
        .is_err());
    assert_eq!(other_model.count().await.unwrap(), 0);
}
