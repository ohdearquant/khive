//! A task whose description exceeds the embedder input budget is still fully
//! created: its `depends_on` edges are recorded and the response discloses the
//! truncated embedding input instead of failing after the task is committed.

mod common;

use common::{assign, pack, rt};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct TruncationEmbeddingService {
    calls: Arc<AtomicUsize>,
    dimensions: usize,
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for TruncationEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(vec![vec![1.0; self.dimensions]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "gtd-truncation-test"
    }
}

struct TruncationEmbedderProvider {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    dimensions: usize,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for TruncationEmbedderProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError>
    {
        Ok(Arc::new(TruncationEmbeddingService {
            calls: self.calls.clone(),
            dimensions: self.dimensions,
        }))
    }
}

fn register_embedder(rt: &khive_runtime::KhiveRuntime, name: &'static str) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let dimensions = if name == "multilingual-e5-small" {
        lattice_embed::EmbeddingModel::MultilingualE5Small.dimensions()
    } else {
        1
    };
    rt.register_embedder(TruncationEmbedderProvider {
        name,
        calls: calls.clone(),
        dimensions,
    });
    calls
}

#[tokio::test]
async fn assign_over_embedding_budget_records_dependencies_and_discloses_truncation() {
    use khive_storage::types::{Direction, NeighborQuery};
    use khive_storage::EdgeRelation;

    let rt = rt();
    register_embedder(&rt, "gtd-truncation-test");
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
async fn keyed_assign_discloses_truncation_on_fresh_write_and_replay_without_embedding() {
    let rt = rt();
    let calls = register_embedder(&rt, "gtd-truncation-test");
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

    let calls_before_replay = calls.load(Ordering::SeqCst);
    assert!(
        calls_before_replay > 0,
        "the fresh write must embed content"
    );
    let replayed = assign(&pack, args).await;
    assert_eq!(
        replayed["replayed"],
        json!(true),
        "second call replays: {replayed}"
    );
    assert_eq!(replayed["full_id"], created["full_id"]);
    assert_eq!(
        replayed["warnings"], created["warnings"],
        "an identical retry must retain the embedding truncation disclosure: {replayed}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before_replay,
        "replay must disclose truncation without embedding again"
    );
}

#[tokio::test]
async fn keyed_assign_replay_warning_respects_model_budget_and_no_embeddings() {
    for (model, content_bytes, truncated) in [
        (
            Some("gtd-truncation-test"),
            lattice_embed::MAX_TEXT_BYTES,
            false,
        ),
        (
            Some("multilingual-e5-small"),
            khive_runtime::retrieval::document_embedding_budget("multilingual-e5-small"),
            false,
        ),
        (
            Some("multilingual-e5-small"),
            khive_runtime::retrieval::document_embedding_budget("multilingual-e5-small") + 1,
            true,
        ),
        (None, lattice_embed::MAX_TEXT_BYTES + 1, false),
    ] {
        let rt = rt();
        let calls = model.map(|name| register_embedder(&rt, name));
        let pack = pack(rt.clone());
        let args = json!({
            "title": "budget boundary task",
            "description": "x".repeat(content_bytes),
            "idempotency_key": "budget-boundary",
        });
        let created = assign(&pack, args.clone()).await;
        let calls_before_replay = calls.as_ref().map(|count| count.load(Ordering::SeqCst));
        let replayed = assign(&pack, args).await;

        assert_eq!(replayed["full_id"], created["full_id"]);
        assert_eq!(replayed["replayed"], json!(true));
        for response in [&created, &replayed] {
            assert_eq!(
                response.get("warnings").is_some(),
                truncated,
                "model={model:?}, content_bytes={content_bytes}: {response}"
            );
        }
        assert_eq!(
            calls.as_ref().map(|count| count.load(Ordering::SeqCst)),
            calls_before_replay,
            "model={model:?}: replay must not embed again"
        );
        let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
        let stored = rt
            .notes(&token)
            .unwrap()
            .get_live_notes_by_key("local", "budget-boundary", Some("task"))
            .await
            .unwrap();
        assert_eq!(stored.len(), 1, "replay must not create another task");
        assert_eq!(stored[0].content.len(), content_bytes);
    }
}

#[tokio::test]
async fn keyed_assign_replay_discloses_model_prefix_truncation() {
    let model = lattice_embed::EmbeddingModel::MultilingualE5Small;
    let prefix_bytes = model.document_instruction().unwrap().len();
    assert_eq!(prefix_bytes, 9);
    let content_bytes = lattice_embed::MAX_TEXT_BYTES - prefix_bytes + 1;
    assert!(content_bytes < lattice_embed::MAX_TEXT_BYTES);

    let rt = rt();
    let calls = register_embedder(&rt, "multilingual-e5-small");
    let pack = pack(rt.clone());
    let args = json!({
        "title": "E5 passage prefix budget",
        "description": "x".repeat(content_bytes),
        "idempotency_key": "e5-prefix-truncation",
    });
    let created = assign(&pack, args.clone()).await;
    assert_eq!(
        created["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "the passage prefix must consume the fresh embedding budget: {created}"
    );
    let calls_before_replay = calls.load(Ordering::SeqCst);
    assert!(calls_before_replay > 0);
    let replayed = assign(&pack, args).await;
    assert_eq!(replayed["full_id"], created["full_id"]);
    assert_eq!(replayed["replayed"], json!(true));
    assert_eq!(
        replayed["warnings"], created["warnings"],
        "replay must deduct the passage prefix from the embedding budget: {replayed}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), calls_before_replay);
}

#[tokio::test]
async fn keyed_assign_replay_warning_ignores_models_excluded_by_task_policy() {
    let rt = rt();
    let calls = register_embedder(&rt, "gtd-truncation-test");
    rt.install_note_embedding_policies(&[khive_runtime::NoteEmbeddingPolicySpec {
        kind: "task",
        policy: khive_runtime::NoteEmbeddingPolicy::DefaultModel,
    }]);
    assert!(!rt.registered_embedding_model_names().is_empty());
    assert!(rt.embedding_models_for_note_kind("task").is_empty());
    let pack = pack(rt.clone());
    let args = json!({
        "title": "task with no selected embedding model",
        "description": "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1),
        "idempotency_key": "task-embedding-policy",
    });
    let created = assign(&pack, args.clone()).await;
    let replayed = assign(&pack, args).await;

    assert_eq!(replayed["full_id"], created["full_id"]);
    assert_eq!(replayed["replayed"], json!(true));
    assert!(created.get("warnings").is_none());
    assert!(
        replayed.get("warnings").is_none(),
        "an unselected model must not produce a truncation warning: {replayed}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
