//! Public sqlite-vec search boundaries, including a real file close/reopen.
#![cfg(feature = "vectors")]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::stores::vectors::SqliteVecStore;
use khive_storage::{
    StorageCapability, StorageError, VectorSearchHit, VectorSearchRequest, VectorStore,
};
use khive_types::SubstrateKind;
use uuid::Uuid;

const MODEL: &str = "tie-order-model";

fn open(path: &Path) -> (Arc<ConnectionPool>, SqliteVecStore) {
    khive_db::extension::ensure_extensions_loaded();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.to_owned()),
            max_readers: 2,
            write_queue_enabled: Some(false),
            write_routing_strict: false,
            write_admission_deadline_ms: 2_000,
            busy_timeout: Duration::from_secs(5),
            checkout_timeout: Duration::from_secs(5),
            disk_guard_config: Some(Default::default()),
            volume_lock_dir: Some(path.parent().unwrap().join("locks")),
            ..PoolConfig::for_test()
        })
        .expect("private file-backed pool"),
    );
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(khive_db::migrations::ANN_WRITE_LOG_DDL)
            .unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS vec_tie_order USING vec0(\
             subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, \
             field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
             embedding float[3] distance_metric=cosine)",
            )
            .unwrap();
    }
    let store = store(&pool, MODEL);
    (pool, store)
}

fn store(pool: &Arc<ConnectionPool>, model: &str) -> SqliteVecStore {
    SqliteVecStore::new(
        pool.clone(),
        true,
        "tie_order".into(),
        model.into(),
        3,
        "local".into(),
    )
    .unwrap()
}

fn request(k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        query_vectors: vec![vec![1.0, 0.0, 0.0]],
        top_k: k,
        namespace: None,
        kind: Some(SubstrateKind::Note),
        embedding_model: None,
        filter: None,
        backend_hints: None,
    }
}

async fn insert(store: &SqliteVecStore, id: u128, ns: &str, kind: SubstrateKind, vector: [f32; 3]) {
    store
        .insert(
            Uuid::from_u128(id),
            kind,
            ns,
            "note.content",
            vec![vector.to_vec()],
        )
        .await
        .unwrap();
}

fn rows(hits: &[VectorSearchHit]) -> Vec<(Uuid, f64, u32)> {
    hits.iter()
        .map(|h| (h.subject_id, h.score.to_f64(), h.rank))
        .collect()
}

#[tokio::test]
async fn tied_boundary_is_id_ordered_before_and_after_reopen_in_both_insert_orders() {
    for order in [[10, 20, 30, 40, 50, 60], [60, 20, 50, 10, 40, 30]] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vectors.db");
        let (pool, store) = open(&path);
        for id in order {
            insert(&store, id, "local", SubstrateKind::Note, [1.0, 0.0, 0.0]).await;
        }
        // Lower IDs in other metadata scopes must not consume the boundary.
        insert(&store, 1, "hidden", SubstrateKind::Note, [1.0, 0.0, 0.0]).await;
        insert(&store, 2, "local", SubstrateKind::Entity, [1.0, 0.0, 0.0]).await;
        let other = self::store(&pool, "other-model");
        insert(&other, 3, "local", SubstrateKind::Note, [1.0, 0.0, 0.0]).await;
        drop(other);
        let expected = vec![
            (Uuid::from_u128(10), 1.0, 1),
            (Uuid::from_u128(20), 1.0, 2),
            (Uuid::from_u128(30), 1.0, 3),
        ];
        let before = rows(&store.search(request(3)).await.unwrap());
        assert_eq!(before, expected);
        assert!(store.search(request(0)).await.unwrap().is_empty());
        assert_eq!(store.search(request(20)).await.unwrap().len(), 6);
        // The scalar route intentionally has no MATCH engine's incidental K_MAX.
        assert_eq!(store.search(request(4097)).await.unwrap().len(), 6);
        let mut override_namespace = request(3);
        override_namespace.namespace = Some("hidden".into());
        assert_eq!(
            rows(&store.search(override_namespace).await.unwrap()),
            vec![(Uuid::from_u128(1), 1.0, 1)]
        );
        let mut override_model = request(3);
        override_model.embedding_model = Some("other-model".into());
        assert_eq!(
            rows(&store.search(override_model).await.unwrap()),
            vec![(Uuid::from_u128(3), 1.0, 1)]
        );
        let mut empty = request(3);
        empty.namespace = Some("empty".into());
        assert!(store.search(empty).await.unwrap().is_empty());
        drop(store);
        drop(pool);
        let (pool, store) = open(&path);
        let after = rows(&store.search(request(3)).await.unwrap());
        assert_eq!(after, expected);
        assert_eq!(after, before);
        drop(store);
        drop(pool);
    }
}

#[tokio::test]
async fn raw_distance_precedes_id_and_preserves_signed_cosine() {
    let dir = tempfile::tempdir().unwrap();
    let (_pool, store) = open(&dir.path().join("numeric.db"));
    for (id, vector) in [
        (1, [-1.0, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [3.0, 4.0, 0.0]),
        (99, [1.0, 0.0, 0.0]),
    ] {
        insert(&store, id, "local", SubstrateKind::Note, vector).await;
    }
    let hits = store.search(request(4)).await.unwrap();
    assert_eq!(
        hits.iter().map(|h| h.subject_id).collect::<Vec<_>>(),
        [99, 3, 2, 1].map(Uuid::from_u128)
    );
    for (hit, expected) in hits.iter().zip([1.0, 0.6, 0.0, -1.0]) {
        assert!((hit.score.to_f64() - expected).abs() < 1e-6);
    }
    assert_eq!(
        hits.iter().map(|h| h.rank).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
}

#[tokio::test]
async fn malformed_or_undefined_cosines_are_errors_not_successful_hits() {
    let dir = tempfile::tempdir().unwrap();
    let (_pool, store) = open(&dir.path().join("invalid.db"));
    insert(&store, 10, "local", SubstrateKind::Note, [1.0, 0.0, 0.0]).await;
    let mut wrong_dimensions = request(1);
    wrong_dimensions.query_vectors = vec![vec![1.0, 0.0]];
    let error = store.search(wrong_dimensions).await.unwrap_err();
    match error {
        StorageError::Driver {
            capability,
            operation,
            source,
        } => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(operation, "vec_search");
            assert!(matches!(
                source.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::InvalidParameterCount(2, 3))
            ));
        }
        other => panic!("dimension driver error: {other:?}"),
    }
    let mut non_finite = request(1);
    non_finite.query_vectors[0][1] = f32::NAN;
    assert!(matches!(
        store.search(non_finite).await.unwrap_err(),
        StorageError::InvalidInput {
            capability: StorageCapability::Vectors,
            ..
        }
    ));
    let mut zero = request(1);
    zero.query_vectors = vec![vec![0.0; 3]];
    assert!(matches!(
        store.search(zero).await.unwrap_err(),
        StorageError::Driver { .. }
    ));
    // Finite f32 values can overflow the cosine accumulator. This combination
    // produces an undefined distance; it must not be converted into a hit.
    insert(&store, 20, "local", SubstrateKind::Note, [f32::MAX; 3]).await;
    let mut overflow = request(2);
    overflow.query_vectors = vec![vec![f32::MAX; 3]];
    assert!(matches!(
        store.search(overflow).await.unwrap_err(),
        StorageError::Driver { .. }
    ));
}
