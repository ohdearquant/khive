//! Runtime entity eligibility is applied before either SQLite vector window.
#![cfg(feature = "vectors")]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::stores::vectors::SqliteVecStore;
use khive_storage::{VectorSearchHit, VectorSearchRequest, VectorStore};
use khive_types::SubstrateKind;
use uuid::Uuid;

fn store(path: &Path) -> SqliteVecStore {
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
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .autocommit(|conn| {
            conn.execute_batch(khive_db::migrations::ANN_WRITE_LOG_DDL)?;
            conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_entity_arm USING vec0(\
                 subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, \
                 field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
                 embedding float[3] distance_metric=cosine)",
            )?;
            Ok(())
        })
        .unwrap();
    SqliteVecStore::new(
        pool,
        true,
        "entity_arm".into(),
        "entity-arm-model".into(),
        3,
        "local".into(),
    )
    .unwrap()
}

fn request(top_k: u32, eligible_entities: bool) -> VectorSearchRequest {
    VectorSearchRequest {
        query_vectors: vec![vec![1.0, 0.0, 0.0]],
        top_k,
        namespace: Some("local".into()),
        kind: Some(SubstrateKind::Entity),
        embedding_model: None,
        filter: None,
        backend_hints: eligible_entities
            .then(|| serde_json::json!({ "exclude_knowledge_atoms": true })),
    }
}

async fn populate(store: &SqliteVecStore) {
    for id in 1..=50 {
        store
            .insert(
                Uuid::from_u128(id),
                SubstrateKind::Entity,
                "local",
                "knowledge.atom",
                vec![vec![1.0, 0.0, 0.0]],
            )
            .await
            .unwrap();
    }
    for id in 101..=105 {
        store
            .insert(
                Uuid::from_u128(id),
                SubstrateKind::Entity,
                "local",
                if id == 105 {
                    "entity.title"
                } else {
                    "entity.body"
                },
                vec![vec![0.8, 0.6, 0.0]],
            )
            .await
            .unwrap();
    }
}

fn ids(hits: &[VectorSearchHit]) -> Vec<Uuid> {
    hits.iter().map(|hit| hit.subject_id).collect()
}

#[tokio::test]
async fn entity_arm_excludes_atoms_before_match_and_scalar_windows() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir.path().join("entity_windows.db"));
    populate(&store).await;

    let expected: Vec<_> = (101..=105).map(Uuid::from_u128).collect();
    // Twenty closer atom vectors fill an ordinary entity-tagged KNN window.
    let generic = store.search(request(20, false)).await.unwrap();
    assert_eq!(generic.len(), 20);
    assert!(generic.iter().all(|hit| hit.subject_id.as_u128() <= 50));

    // 4097 exceeds MATCH's native cap and exercises the scalar fallback.
    for top_k in [1, 20, 4097] {
        let hits = store.search(request(top_k, true)).await.unwrap();
        let count = (top_k as usize).min(expected.len());
        assert_eq!(ids(&hits), expected[..count], "top_k={top_k}");
        for (index, hit) in hits.iter().enumerate() {
            assert_eq!(hit.rank, index as u32 + 1);
            assert!((hit.score.to_f64() - 0.8).abs() < 0.000_001);
        }
    }
}

#[tokio::test]
async fn entity_eligibility_hint_preserves_generic_and_unfiltered_searches() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir.path().join("generic_vectors.db"));
    populate(&store).await;

    let generic = store.search(request(20, false)).await.unwrap();
    let mut disabled = request(20, true);
    disabled.backend_hints = Some(serde_json::json!({ "exclude_knowledge_atoms": false }));
    assert_eq!(ids(&store.search(disabled).await.unwrap()), ids(&generic));

    let mut unfiltered = request(20, true);
    unfiltered.kind = None;
    assert_eq!(ids(&store.search(unfiltered).await.unwrap()), ids(&generic));

    let mut not_a_boolean = request(20, true);
    not_a_boolean.backend_hints = Some(serde_json::json!({ "exclude_knowledge_atoms": "true" }));
    assert_eq!(
        ids(&store.search(not_a_boolean).await.unwrap()),
        ids(&generic)
    );
}
