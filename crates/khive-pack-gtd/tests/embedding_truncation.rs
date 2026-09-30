//! A task whose description exceeds the embedder input budget is still fully
//! created: its `depends_on` edges are recorded and the response discloses the
//! truncated embedding input instead of failing after the task is committed.

mod common;

use common::{assign, pack, rt};
use serde_json::json;

struct TruncationEmbeddingService;

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for TruncationEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(vec![vec![1.0]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "gtd-truncation-test"
    }
}

struct TruncationEmbedderProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for TruncationEmbedderProvider {
    fn name(&self) -> &str {
        "gtd-truncation-test"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError>
    {
        Ok(std::sync::Arc::new(TruncationEmbeddingService))
    }
}

#[tokio::test]
async fn assign_over_embedding_budget_records_dependencies_and_discloses_truncation() {
    use khive_storage::types::{Direction, NeighborQuery};
    use khive_storage::EdgeRelation;

    let rt = rt();
    rt.register_embedder(TruncationEmbedderProvider);
    let pack = pack(rt.clone());

    let blocker = assign(&pack, json!({"title": "write spec"})).await;
    let blocker_full = blocker["full_id"].as_str().unwrap();
    let dependent = assign(
        &pack,
        json!({
            "title": "implement feature",
            "description": "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1),
            "depends_on": [blocker_full],
        }),
    )
    .await;

    assert_eq!(
        dependent["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "assign must disclose the truncated embedding input: {dependent}"
    );

    let dependent_id = uuid::Uuid::parse_str(dependent["full_id"].as_str().unwrap()).unwrap();
    let blocker_id = uuid::Uuid::parse_str(blocker_full).unwrap();
    let graph = rt
        .graph(&rt.authorize(khive_runtime::Namespace::local()).unwrap())
        .expect("graph store");
    let targets: Vec<_> = graph
        .neighbors(
            dependent_id,
            NeighborQuery {
                direction: Direction::Out,
                relations: Some(vec![EdgeRelation::DependsOn]),
                limit: Some(16),
                min_weight: None,
            },
        )
        .await
        .expect("neighbors query")
        .iter()
        .map(|hit| hit.node_id)
        .collect();
    assert!(
        targets.contains(&blocker_id),
        "the committed task must still get its depends_on edge; got {targets:?}"
    );
}

#[tokio::test]
async fn keyed_assign_discloses_truncation_on_fresh_write_and_not_on_replay() {
    let rt = rt();
    rt.register_embedder(TruncationEmbedderProvider);
    let pack = pack(rt.clone());

    let args = json!({
        "title": "keyed oversized task",
        "description": "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1),
        "idempotency_key": "keyed-truncation",
    });

    let created = assign(&pack, args.clone()).await;
    assert!(
        created.get("replayed").is_none(),
        "first call is a fresh write: {created}"
    );
    assert_eq!(
        created["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "a fresh keyed assign must disclose the truncated embedding input: {created}"
    );

    let replayed = assign(&pack, args).await;
    assert_eq!(
        replayed["replayed"],
        json!(true),
        "second call replays: {replayed}"
    );
    assert_eq!(replayed["full_id"], created["full_id"]);
    assert!(
        replayed.get("warnings").is_none(),
        "a replay embeds nothing, so it must carry no truncation warning: {replayed}"
    );
}
