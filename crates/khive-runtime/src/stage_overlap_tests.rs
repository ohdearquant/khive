//! Entity search and note search run their text stage and their vector stage together.
//!
//! The text stage is held by `TEXT_STAGE_DOUBLE`; the vector stage is the query embedding of
//! a registered embedder. A barrier of two parties, one in each, releases only when both
//! stages are in flight at once.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_score::DeterministicScore;
use khive_storage::StorageError;
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use tokio::sync::Barrier;
use uuid::Uuid;

use crate::operations::arm_fts_search_fail;
use crate::retrieval::SearchHit;
use crate::stage_seam::{BoxedTextStage, TextStageDouble, TEXT_STAGE_DOUBLE};
use crate::{
    EmbedderProvider, KhiveRuntime, NamespaceToken, RuntimeConfig, RuntimeError, RuntimeResult,
    SearchSignals, SearchSource,
};

/// Long enough that a loaded machine still releases a barrier both stages reach, short enough
/// that a search whose stages never overlap fails well inside a minute.
const STAGE_WAIT: Duration = Duration::from_secs(10);

/// How long the vector stage takes before it fails in the error-precedence tests.
const SLOW_VECTOR: Duration = Duration::from_millis(200);

struct StageEmbeddingService {
    dimensions: usize,
    barrier: Option<Arc<Barrier>>,
    delay: Duration,
    fail: bool,
}

#[async_trait]
impl EmbeddingService for StageEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(EmbedError::ModelInitialization(
                "injected slow vector stage failure".to_string(),
            ));
        }
        Ok(texts.iter().map(|_| vec![1.0; self.dimensions]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "stage-overlap-test-embedding"
    }
}

/// An embedder whose query embedding can wait at a barrier, take time, and fail.
struct StageEmbedderProvider {
    name: String,
    dimensions: usize,
    barrier: Option<Arc<Barrier>>,
    delay: Duration,
    fail: bool,
}

impl StageEmbedderProvider {
    fn healthy() -> Self {
        let model = EmbeddingModel::AllMiniLmL6V2;
        Self {
            name: model.to_string(),
            dimensions: model.dimensions(),
            barrier: None,
            delay: Duration::ZERO,
            fail: false,
        }
    }
}

#[async_trait]
impl EmbedderProvider for StageEmbedderProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(StageEmbeddingService {
            dimensions: self.dimensions,
            barrier: self.barrier.clone(),
            delay: self.delay,
            fail: self.fail,
        }))
    }
}

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    runtime.register_embedder(StageEmbedderProvider::healthy());
    runtime
}

/// A text stage that waits at `barrier` and then finds nothing.
fn double_waiting_at(barrier: Arc<Barrier>) -> TextStageDouble {
    Arc::new(move || -> BoxedTextStage {
        let barrier = Arc::clone(&barrier);
        Box::pin(async move {
            barrier.wait().await;
            Ok(Vec::new())
        })
    })
}

/// A text stage that fails the way a backend outage does.
fn double_failing_with_timeout() -> TextStageDouble {
    Arc::new(|| -> BoxedTextStage {
        Box::pin(async {
            Err(StorageError::Timeout {
                operation: "fts_search".into(),
            })
        })
    })
}

fn summary(hits: &[SearchHit]) -> Vec<(Uuid, DeterministicScore, SearchSignals, SearchSource)> {
    let mut rows = Vec::new();
    for hit in hits {
        rows.push((hit.entity_id, hit.score, hit.signals, hit.source));
    }
    rows
}

#[tokio::test]
async fn entity_search_runs_its_text_and_vector_stages_together() {
    let rt = runtime();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "FlashAttention",
        Some("IO-aware exact attention using tiling"),
        None,
        vec![],
    )
    .await
    .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    rt.register_embedder(StageEmbedderProvider {
        barrier: Some(Arc::clone(&barrier)),
        ..StageEmbedderProvider::healthy()
    });
    let search = rt.hybrid_search(&tok, "FlashAttention", None, 10, None, None, &[], None);
    let search = TEXT_STAGE_DOUBLE.scope(double_waiting_at(Arc::clone(&barrier)), search);

    let outcome = tokio::time::timeout(STAGE_WAIT, search).await;
    assert!(
        outcome.is_ok(),
        "the text stage and the vector stage never ran at the same time"
    );
    outcome.unwrap().expect("the search succeeds");
}

#[tokio::test]
async fn note_search_runs_its_text_and_vector_stages_together() {
    let rt = runtime();
    let tok = NamespaceToken::local();
    rt.create_note(
        &tok,
        "observation",
        None,
        "FlashAttention reduces memory by using tiling",
        Some(0.8),
        None,
        vec![],
    )
    .await
    .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    rt.register_embedder(StageEmbedderProvider {
        barrier: Some(Arc::clone(&barrier)),
        ..StageEmbedderProvider::healthy()
    });
    let search = rt.search_notes(&tok, "FlashAttention", None, 10, None, false, &[], None);
    let search = TEXT_STAGE_DOUBLE.scope(double_waiting_at(Arc::clone(&barrier)), search);

    let outcome = tokio::time::timeout(STAGE_WAIT, search).await;
    assert!(
        outcome.is_ok(),
        "the text stage and the vector stage never ran at the same time"
    );
    outcome.unwrap().expect("the search succeeds");
}

#[tokio::test]
async fn entity_search_reports_the_text_error_when_the_slow_vector_stage_also_fails() {
    let rt = runtime();
    let tok = NamespaceToken::local();
    rt.register_embedder(StageEmbedderProvider {
        delay: SLOW_VECTOR,
        fail: true,
        ..StageEmbedderProvider::healthy()
    });
    let search = rt.hybrid_search(&tok, "FlashAttention", None, 10, None, None, &[], None);

    let err = TEXT_STAGE_DOUBLE
        .scope(double_failing_with_timeout(), search)
        .await
        .expect_err("a failed text stage fails the search");
    assert!(
        matches!(err, RuntimeError::Storage(StorageError::Timeout { .. })),
        "the text error must be the one reported, got {err:?}"
    );
}

#[tokio::test]
async fn note_search_reports_the_text_error_when_the_slow_vector_stage_also_fails() {
    let rt = runtime();
    let ns = Namespace::parse("stage-overlap-notes").unwrap();
    let tok = rt.authorize(ns.clone()).unwrap();
    rt.register_embedder(StageEmbedderProvider {
        delay: SLOW_VECTOR,
        fail: true,
        ..StageEmbedderProvider::healthy()
    });
    arm_fts_search_fail(ns.as_str());

    let err = rt
        .search_notes(&tok, "FlashAttention", None, 10, None, false, &[], None)
        .await
        .expect_err("a failed text stage fails the search");
    assert!(
        matches!(err, RuntimeError::Storage(StorageError::Timeout { .. })),
        "the text error must be the one reported, got {err:?}"
    );
}

#[tokio::test]
async fn entity_search_matches_the_stage_by_stage_search_on_a_fixed_fixture() {
    let rt = runtime();
    let tok = NamespaceToken::local();
    for (name, text) in [
        ("FlashAttention", "IO-aware exact attention using tiling"),
        (
            "FlashDecoding",
            "Split-KV decoding kernel for long contexts",
        ),
        ("PagedAttention", "Paged KV cache memory management"),
    ] {
        rt.create_entity(&tok, "concept", None, name, Some(text), None, vec![])
            .await
            .unwrap();
    }

    let joined = rt
        .hybrid_search(
            &tok,
            "FlashAttention",
            None,
            10,
            Some("concept"),
            None,
            &[],
            None,
        )
        .await
        .unwrap();
    let sequential = rt
        .hybrid_search_each_kind(&tok, "FlashAttention", None, 10, &["concept"])
        .await
        .unwrap();

    assert_eq!(
        summary(&joined),
        summary(&sequential[0]),
        "the overlapped stages must give the stage-by-stage ids, scores and signals"
    );
    assert!(!joined.is_empty(), "premise: the fixture matches");
    assert!(
        joined[0].signals.keyword_score.is_some(),
        "premise: the text stage contributes to the top hit"
    );
    assert!(
        joined[0].signals.vector_similarity.is_some(),
        "premise: the vector stage contributes to the top hit"
    );
}

#[tokio::test]
async fn note_search_keeps_the_hits_of_both_stages() {
    let rt = runtime();
    let tok = NamespaceToken::local();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "FlashAttention reduces memory by using tiling",
            Some(0.8),
            None,
            vec![],
        )
        .await
        .unwrap();

    let hits = rt
        .search_notes(&tok, "FlashAttention", None, 10, None, false, &[], None)
        .await
        .unwrap();

    assert_eq!(hits.len(), 1, "the one matching note is returned");
    assert_eq!(hits[0].note_id, note.id);
    assert_eq!(hits[0].source, SearchSource::Both);
    assert!(hits[0].signals.keyword_score.is_some());
    assert!(hits[0].signals.vector_similarity.is_some());
}
