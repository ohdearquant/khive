use std::sync::Arc;

use khive_storage::VectorStore;
use khive_types::SubstrateKind;
use uuid::Uuid;

use super::*;
use crate::pool::{ConnectionPool, PoolConfig};

fn file_pool(path: &std::path::Path) -> Arc<ConnectionPool> {
    crate::extension::ensure_extensions_loaded();
    Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.to_owned()),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .expect("file-backed vector pool"),
    )
}

fn create_vec_table(pool: &Arc<ConnectionPool>, model_key: &str, dims: usize) {
    let ddl = format!(
        "CREATE VIRTUAL TABLE vec_{model_key} USING vec0(\
         subject_id TEXT PRIMARY KEY, \
         namespace TEXT NOT NULL, \
         kind TEXT NOT NULL, \
         field TEXT NOT NULL, \
         embedding_model TEXT NOT NULL, \
         embedding float[{dims}] distance_metric=cosine)"
    );
    let writer = pool.try_writer().expect("vector writer");
    writer.conn().execute_batch(&ddl).expect("vec0 table");
    writer
        .conn()
        .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
        .expect("provenance sidecar");
    writer
        .conn()
        .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
        .expect("ANN write log");
}

#[tokio::test]
async fn get_vectors_reads_persisted_requested_rows_with_exact_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stored-vectors.db");
    let model_key = "stored_vector_read";
    let stored = vec![0.125_f32, -0.75, 3.5, 0.1];
    let wanted = Uuid::from_u128(1);
    let foreign_namespace = Uuid::from_u128(2);
    let other_field = Uuid::from_u128(3);
    let missing = Uuid::from_u128(4);
    let unrequested = Uuid::from_u128(5);

    {
        let pool = file_pool(&path);
        create_vec_table(&pool, model_key, stored.len());
        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            true,
            model_key.into(),
            model_key.into(),
            stored.len(),
            "local".into(),
        )
        .expect("store");
        assert!(store.capabilities().supports_vector_read);

        for (id, namespace, field, vector) in [
            (wanted, "local", "knowledge.atom", stored.clone()),
            (
                foreign_namespace,
                "other",
                "knowledge.atom",
                vec![0.0, 1.0, 0.0, 0.0],
            ),
            (
                other_field,
                "local",
                "other.field",
                vec![0.0, 0.0, 1.0, 0.0],
            ),
            (
                unrequested,
                "local",
                "knowledge.atom",
                vec![0.0, 0.0, 0.0, 1.0],
            ),
        ] {
            store
                .insert(id, SubstrateKind::Entity, namespace, field, vec![vector])
                .await
                .expect("seed stored vector");
        }

        let ids = [wanted, foreign_namespace, other_field, missing];
        let found = store
            .get_vectors(&ids, "local", "knowledge.atom")
            .await
            .expect("point read");
        assert_eq!(found.len(), 1, "only the requested matching row returns");
        assert_eq!(found.get(&wanted), Some(&stored));
        assert!(!found.contains_key(&unrequested));

        let other_model = SqliteVecStore::new(
            Arc::clone(&pool),
            true,
            model_key.into(),
            "different_model".into(),
            stored.len(),
            "local".into(),
        )
        .expect("other model view");
        assert!(
            other_model
                .get_vectors(&[wanted], "local", "knowledge.atom")
                .await
                .expect("model-scoped read")
                .is_empty(),
            "the same vec0 table cannot leak a different embedding model"
        );
    }

    // Reopening the file without constructing any ANN index proves this reads
    // persisted vec0 bytes, including the non-round decimal component.
    let pool = file_pool(&path);
    let reopened = SqliteVecStore::new(
        pool,
        true,
        model_key.into(),
        model_key.into(),
        stored.len(),
        "local".into(),
    )
    .expect("reopened store");
    let found = reopened
        .get_vectors(&[wanted], "local", "knowledge.atom")
        .await
        .expect("reopened point read");
    assert_eq!(found.get(&wanted), Some(&stored));
}

#[tokio::test]
async fn get_vectors_empty_ids_does_not_open_an_uncreated_vec0_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = file_pool(&dir.path().join("empty.db"));
    let store = SqliteVecStore::new(
        pool,
        true,
        "never_created".into(),
        "never_created".into(),
        4,
        "local".into(),
    )
    .expect("store");
    assert!(store
        .get_vectors(&[], "local", "knowledge.atom")
        .await
        .expect("empty lookup")
        .is_empty());
}
