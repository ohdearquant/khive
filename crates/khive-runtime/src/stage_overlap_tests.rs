//! Entity search and note search run their text stage and their vector stage together.
//!
//! The text stage is held by `TEXT_STAGE_DOUBLE`; the vector stage is the query embedding of
//! a registered embedder. A barrier of two parties, one in each, releases only when both
//! stages are in flight at once.

use std::sync::{Arc, Mutex};
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

/// What the vector stage does on its next query embedding. A test sets it after the fixture
/// is in place: the registry refuses to replace a provider that has already served, so the
/// one provider registered before setup stays and changes behaviour instead.
#[derive(Clone, Default)]
struct StageBehavior {
    barrier: Option<Arc<Barrier>>,
    delay: Duration,
    fail: bool,
}

/// Shared handle on the registered provider's behaviour.
#[derive(Clone, Default)]
struct StageControl(Arc<Mutex<StageBehavior>>);

impl StageControl {
    fn set(&self, behavior: StageBehavior) {
        *self.0.lock().expect("stage behaviour lock") = behavior;
    }

    fn current(&self) -> StageBehavior {
        self.0.lock().expect("stage behaviour lock").clone()
    }
}

struct StageEmbeddingService {
    dimensions: usize,
    control: StageControl,
}

#[async_trait]
impl EmbeddingService for StageEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let StageBehavior {
            barrier,
            delay,
            fail,
        } = self.control.current();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
        tokio::time::sleep(delay).await;
        if fail {
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

/// An embedder whose query embedding can wait at a barrier, take time, and fail, as its
/// control says at the time of the call.
struct StageEmbedderProvider {
    name: String,
    dimensions: usize,
    control: StageControl,
}

impl StageEmbedderProvider {
    fn controlled_by(control: StageControl) -> Self {
        let model = EmbeddingModel::AllMiniLmL6V2;
        Self {
            name: model.to_string(),
            dimensions: model.dimensions(),
            control,
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
            control: self.control.clone(),
        }))
    }
}

/// An in-memory runtime whose one embedder starts healthy, with the handle that changes it.
fn runtime() -> (KhiveRuntime, StageControl) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let control = StageControl::default();
    runtime.register_embedder(StageEmbedderProvider::controlled_by(control.clone()));
    (runtime, control)
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
    let (rt, vector) = runtime();
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
    vector.set(StageBehavior {
        barrier: Some(Arc::clone(&barrier)),
        ..StageBehavior::default()
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
    let (rt, vector) = runtime();
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
    vector.set(StageBehavior {
        barrier: Some(Arc::clone(&barrier)),
        ..StageBehavior::default()
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
    let (rt, vector) = runtime();
    let tok = NamespaceToken::local();
    vector.set(StageBehavior {
        delay: SLOW_VECTOR,
        fail: true,
        ..StageBehavior::default()
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
    let (rt, vector) = runtime();
    let ns = Namespace::parse("stage-overlap-notes").unwrap();
    let tok = rt.authorize(ns.clone()).unwrap();
    vector.set(StageBehavior {
        delay: SLOW_VECTOR,
        fail: true,
        ..StageBehavior::default()
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
    let (rt, _vector) = runtime();
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
    let (rt, _vector) = runtime();
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
