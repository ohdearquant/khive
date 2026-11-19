//! Native cross-encoder reranking via `lattice-inference`.
//!
//! Provides `NativeCrossEncoderReranker<Id, R, S>` which implements `Reranker<Id>`.
//! Document texts are fetched by `RerankDocumentResolver<Id>` so the existing
//! `Reranker` trait (which only carries IDs and scores) does not need to change.

use std::marker::PhantomData;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::{Result, RetrievalError};
use crate::hybrid::searcher::Reranker;

/// The locked native engine's hook contract and geometry types.
pub use lattice_inference::lora_hook::{
    BertLoraGeometry, LoraHook, LoraProjectionShape, LoraTargetGeometry, NoopLoraHook,
};
/// Native BERT cross-encoder returning sigmoid probabilities, not raw logits.
pub use lattice_inference::CrossEncoderModel as NativeCrossEncoderModel;

/// Input-ordered model scores and evidence from this scoring invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossEncoderScores {
    /// One probability per supplied document, in input order.
    pub scores: Vec<f32>,
    /// Whether the hook's `apply` method actually ran during this invocation.
    ///
    /// Hook presence, successful validation and declared activity are not
    /// application evidence. This does not assert that an applied delta was nonzero.
    pub hook_applied: bool,
}

/// Ranked candidates and hook application evidence from one rerank invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeRerankResult<Id> {
    /// Surviving candidates in descending model-score order, limited by `top_k`.
    pub results: Vec<(Id, DeterministicScore)>,
    /// Whether the hook's `apply` method ran while scoring these candidates.
    pub hook_applied: bool,
}

// The observer is local to one synchronous native batch. Forward every hook
// operation so recording evidence cannot bypass the engine's validation policy.
struct ObservedHook<'a> {
    inner: &'a dyn LoraHook,
    applied: AtomicBool,
}

impl<'a> ObservedHook<'a> {
    fn new(inner: &'a dyn LoraHook) -> Self {
        Self {
            inner,
            applied: AtomicBool::new(false),
        }
    }
}

impl LoraHook for ObservedHook<'_> {
    fn apply(&self, layer_idx: usize, module: &str, x: &[f32], output: &mut [f32]) {
        self.inner.apply(layer_idx, module, x, output);
        self.applied.store(true, Ordering::Relaxed);
    }

    fn validate_geometry(
        &self,
        geometry: &dyn LoraTargetGeometry,
    ) -> std::result::Result<(), String> {
        self.inner.validate_geometry(geometry)
    }

    fn validate_against(
        &self,
        config: &lattice_inference::model::qwen35_config::Qwen35Config,
    ) -> std::result::Result<(), String> {
        // Preserve validators implemented against the locked engine's legacy API.
        #[allow(deprecated)]
        self.inner.validate_against(config)
    }

    fn validate_against_bert(
        &self,
        num_hidden_layers: usize,
        hidden_size: usize,
        intermediate_size: usize,
    ) -> std::result::Result<(), String> {
        // BERT 0.15.0 invokes this compatibility validator before validate_geometry.
        #[allow(deprecated)]
        self.inner
            .validate_against_bert(num_hidden_layers, hidden_size, intermediate_size)
    }

    fn is_active(&self, layer_idx: usize, module: &str) -> bool {
        self.inner.is_active(layer_idx, module)
    }
}

/// Resolve document texts for a set of candidate IDs.
///
/// Implementors fetch raw document text from whatever backing store is
/// available. A missing document (e.g. deleted after indexing) should be
/// returned as `None`.
#[async_trait]
pub trait RerankDocumentResolver<Id>: Send + Sync
where
    Id: Send + Sync + 'static,
{
    /// Fetch document bodies for `ids` in input order.
    ///
    /// The returned `Vec` must be the same length as `ids`. A missing document
    /// is represented as `None`; the reranker drops that candidate while keeping
    /// every retained ID aligned with its document and score. An all-missing
    /// batch returns empty without scoring. Resolver errors still fail the call.
    async fn resolve_documents(&self, ids: &[Id]) -> Result<Vec<Option<String>>>;
}

/// Synchronous cross-encoder scorer abstraction (for testability).
pub trait CrossEncoderScorer: Send + Sync {
    /// Score a query against a batch of documents in input order.
    /// The native model returns sigmoid probabilities in `[0, 1]`. Finite
    /// precision and saturation can introduce ties even for distinct logits.
    fn score_batch(&self, query: &str, documents: &[&str]) -> Vec<f32>;

    /// Score with a hook borrowed for this invocation only.
    ///
    /// The default preserves legacy scoring for `None` and refuses every `Some`,
    /// including [`NoopLoraHook`]. Opaque scorers cannot establish that a supplied
    /// hook is harmless, so they must opt into actual hook support explicitly.
    fn score_batch_with_hook(
        &self,
        query: &str,
        documents: &[&str],
        hook: Option<&dyn LoraHook>,
    ) -> Result<CrossEncoderScores> {
        if hook.is_some() {
            return Err(RetrievalError::rerank(
                "cross-encoder scorer does not support per-call hooks",
            ));
        }
        Ok(CrossEncoderScores {
            scores: self.score_batch(query, documents),
            hook_applied: false,
        })
    }
}

impl CrossEncoderScorer for NativeCrossEncoderModel {
    fn score_batch(&self, query: &str, documents: &[&str]) -> Vec<f32> {
        NativeCrossEncoderModel::score_batch(self, query, documents)
    }

    fn score_batch_with_hook(
        &self,
        query: &str,
        documents: &[&str],
        hook: Option<&dyn LoraHook>,
    ) -> Result<CrossEncoderScores> {
        let Some(hook) = hook else {
            return Ok(CrossEncoderScores {
                scores: NativeCrossEncoderModel::score_batch(self, query, documents),
                hook_applied: false,
            });
        };
        let observer = ObservedHook::new(hook);
        let scores =
            NativeCrossEncoderModel::score_batch_with_hook(self, query, documents, &observer)
                .map_err(|error| {
                    RetrievalError::rerank(format!("cross-encoder hook scoring failed: {error}"))
                })?;
        Ok(CrossEncoderScores {
            scores,
            hook_applied: observer.applied.load(Ordering::Relaxed),
        })
    }
}

/// Reranker that scores candidates with a native cross-encoder model.
///
/// Missing documents are dropped before applying `top_k`. Equal model scores
/// retain input order. The scorer is generic so callers may also supply another
/// implementation of [`CrossEncoderScorer`].
pub struct NativeCrossEncoderReranker<Id, R, S>
where
    Id: Clone + Send + Sync + 'static,
    R: RerankDocumentResolver<Id>,
    S: CrossEncoderScorer,
{
    model: Arc<S>,
    resolver: Arc<R>,
    _id: PhantomData<fn() -> Id>,
}

impl<Id, R, S> NativeCrossEncoderReranker<Id, R, S>
where
    Id: Clone + Send + Sync + 'static,
    R: RerankDocumentResolver<Id>,
    S: CrossEncoderScorer,
{
    /// Construct from an existing scorer and resolver.
    pub fn new(model: Arc<S>, resolver: Arc<R>) -> Self {
        Self {
            model,
            resolver,
            _id: PhantomData,
        }
    }

    /// Rerank with a native hook borrowed for this call, without retaining it.
    ///
    /// Resolution, missing-document filtering, ties and score conversion match
    /// [`Reranker::rerank`]. Empty input, zero `top_k`, and all-missing documents
    /// skip scoring and return `hook_applied: false`. Direct native batch scoring
    /// still validates a supplied hook even for an empty document slice.
    pub async fn rerank_with_hook(
        &self,
        query: &str,
        results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
        hook: Option<&dyn LoraHook>,
    ) -> Result<NativeRerankResult<Id>> {
        if top_k == 0 || results.is_empty() {
            return Ok(NativeRerankResult {
                results: Vec::new(),
                hook_applied: false,
            });
        }

        let ids: Vec<Id> = results.iter().map(|(id, _)| id.clone()).collect();
        let resolved = self.resolver.resolve_documents(&ids).await?;
        if resolved.len() != results.len() {
            return Err(RetrievalError::rerank(format!(
                "resolver returned {} documents for {} candidates",
                resolved.len(),
                results.len()
            )));
        }

        let retained: Vec<_> = results
            .into_iter()
            .zip(resolved)
            .enumerate()
            .filter_map(|(position, ((id, _), document))| {
                document.map(|document| (position, id, document))
            })
            .collect();
        if retained.is_empty() {
            return Ok(NativeRerankResult {
                results: Vec::new(),
                hook_applied: false,
            });
        }

        let document_refs: Vec<&str> = retained.iter().map(|(_, _, text)| text.as_str()).collect();
        let CrossEncoderScores {
            scores,
            hook_applied,
        } = self
            .model
            .score_batch_with_hook(query, &document_refs, hook)?;
        if scores.len() != retained.len() {
            return Err(RetrievalError::rerank(format!(
                "model returned {} scores for {} candidates",
                scores.len(),
                retained.len()
            )));
        }

        let mut scored: Vec<(usize, Id, f32)> = retained
            .into_iter()
            .zip(scores)
            .map(|((position, id, _), score)| (position, id, score))
            .collect();

        scored.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

        Ok(NativeRerankResult {
            results: scored
                .into_iter()
                .take(top_k)
                .map(|(_, id, score)| (id, DeterministicScore::from_f64(score as f64)))
                .collect(),
            hook_applied,
        })
    }
}

impl<Id, R> NativeCrossEncoderReranker<Id, R, NativeCrossEncoderModel>
where
    Id: Clone + Send + Sync + 'static,
    R: RerankDocumentResolver<Id>,
{
    /// Load a local BERT classification checkpoint and bind its document resolver.
    pub fn from_directory(path: &Path, resolver: Arc<R>) -> Result<Self> {
        let model = NativeCrossEncoderModel::from_directory(path).map_err(|error| {
            RetrievalError::rerank(format!("cross-encoder model load failed: {error}"))
        })?;
        Ok(Self::new(Arc::new(model), resolver))
    }
}

#[async_trait]
impl<Id, R, S> Reranker<Id> for NativeCrossEncoderReranker<Id, R, S>
where
    Id: Clone + Send + Sync + 'static,
    R: RerankDocumentResolver<Id>,
    S: CrossEncoderScorer,
{
    async fn rerank(
        &self,
        query: &str,
        results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
    ) -> Result<Vec<(Id, DeterministicScore)>> {
        Ok(self
            .rerank_with_hook(query, results, top_k, None)
            .await?
            .results)
    }
}

#[cfg(test)]
#[path = "cross_encoder/tests.rs"]
mod tests;
