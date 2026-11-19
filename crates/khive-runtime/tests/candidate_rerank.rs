use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::embedder_registry::EmbedderProvider;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeResult};
use khive_score::DeterministicScore;
use khive_storage::{StorageError, VectorSearchHit};
use khive_types::SubstrateKind;
use lattice_embed::{EmbeddingModel, EmbeddingService};
use uuid::Uuid;

struct StoredVectorProvider(&'static str);

#[async_trait]
impl EmbedderProvider for StoredVectorProvider {
    fn name(&self) -> &str {
        self.0
    }
    fn dimensions(&self) -> usize {
        EmbeddingModel::AllMiniLmL6V2.dimensions()
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        panic!("candidate scoring must not instantiate an embedding service")
    }
}

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::local(),
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        packs: vec![],
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    runtime.register_embedder(StoredVectorProvider("all-minilm-l6-v2"));
    runtime.register_embedder(StoredVectorProvider("other-candidate-model"));
    runtime
}

fn vector(x: f32, y: f32) -> Vec<f32> {
    let mut values = vec![0.0; EmbeddingModel::AllMiniLmL6V2.dimensions()];
    values[0] = x;
    values[1] = y;
    values
}

fn ids(hits: &[VectorSearchHit]) -> Vec<Uuid> {
    hits.iter().map(|hit| hit.subject_id).collect()
}

#[tokio::test]
async fn rerank_scores_every_candidate_before_cutting_and_breaks_ties_by_id() {
    let rt = runtime();
    let token = rt.authorize(Namespace::local()).unwrap();
    let store = rt.vectors(&token).unwrap();
    for (id, field, embedding) in [
        (1, "title", vector(0.0, 1.0)),
        (2, "content", vector(0.0, -1.0)),
        (3, "entity.body", vector(-1.0, 0.0)),
    ] {
        store
            .insert(
                Uuid::from_u128(id),
                SubstrateKind::Entity,
                "local",
                field,
                vec![embedding],
            )
            .await
            .unwrap();
    }
    for id in 100..110 {
        store
            .insert(
                Uuid::from_u128(id),
                SubstrateKind::Entity,
                "local",
                "content",
                vec![vector(1.0, 0.0)],
            )
            .await
            .unwrap();
    }
    let candidates: Vec<_> = [3, 2, 1].map(Uuid::from_u128).into();
    let query = vector(1.0, 0.0);
    let global = rt.knn(&token, query.clone(), 3).await.unwrap();
    assert_eq!(global.len(), 3);
    assert!(
        global
            .iter()
            .all(|hit| !candidates.contains(&hit.subject_id)),
        "control: non-candidates fill namespace top-k"
    );
    let result = rt.rerank(&token, &query, &candidates, 10).await.unwrap();
    assert_eq!(ids(&result), [1, 2, 3].map(Uuid::from_u128));
    assert_eq!(
        result.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        result.iter().map(|hit| hit.score).collect::<Vec<_>>(),
        [
            DeterministicScore::ZERO,
            DeterministicScore::ZERO,
            DeterministicScore::from_f64(-1.0)
        ]
    );
    assert_eq!(
        ids(&rt.rerank(&token, &query, &candidates, 2).await.unwrap()),
        [1, 2].map(Uuid::from_u128)
    );
    let duplicates = candidates.repeat(150);
    let duplicate_result = rt.rerank(&token, &query, &duplicates, 10).await.unwrap();
    assert_eq!(ids(&duplicate_result), ids(&result));
    assert_eq!(
        duplicate_result
            .iter()
            .map(|hit| hit.rank)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(rt.rerank(&token, &query, &[], 3).await.unwrap().is_empty());
    assert!(rt
        .rerank(&token, &query, &candidates, 0)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn rerank_preserves_namespace_model_kind_and_field_scope() {
    let rt = runtime();
    let token = rt.authorize(Namespace::local()).unwrap();
    let store = rt.vectors(&token).unwrap();
    for (id, kind, ns, field) in [
        (11, SubstrateKind::Entity, "local", "custom.entity.field"),
        (12, SubstrateKind::Note, "local", "content"),
        (13, SubstrateKind::Entity, "foreign", "content"),
    ] {
        store
            .insert(Uuid::from_u128(id), kind, ns, field, vec![vector(1.0, 0.0)])
            .await
            .unwrap();
    }
    rt.vectors_for_model(&token, "other-candidate-model")
        .unwrap()
        .insert(
            Uuid::from_u128(14),
            SubstrateKind::Entity,
            "local",
            "content",
            vec![vector(1.0, 0.0)],
        )
        .await
        .unwrap();
    let candidates = [15, 14, 13, 12, 11].map(Uuid::from_u128);
    assert_eq!(
        ids(&rt
            .rerank(&token, &vector(1.0, 0.0), &candidates, 10)
            .await
            .unwrap()),
        [Uuid::from_u128(11)]
    );
    assert_eq!(
        ids(&store
            .score_candidates(&vector(1.0, 0.0), &candidates, Some(SubstrateKind::Note))
            .await
            .unwrap()),
        [Uuid::from_u128(12)]
    );
    assert_eq!(
        ids(&store
            .score_candidates(&vector(1.0, 0.0), &candidates, None)
            .await
            .unwrap()),
        [11, 12].map(Uuid::from_u128)
    );
}

#[tokio::test]
async fn rerank_propagates_invalid_vector_errors() {
    let rt = runtime();
    let token = rt.authorize(Namespace::local()).unwrap();
    for query in [
        vec![],
        vec![1.0],
        vector(f32::NAN, 1.0),
        vector(f32::INFINITY, 1.0),
    ] {
        let error = rt
            .rerank(&token, &query, &[Uuid::from_u128(1)], 1)
            .await
            .expect_err("invalid query");
        assert!(
            matches!(
                error,
                khive_runtime::RuntimeError::Storage(StorageError::InvalidInput { .. })
            ),
            "{error:?}"
        );
    }
}
