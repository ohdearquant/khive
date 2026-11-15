//! Native cross-encoder reranking via `lattice-inference`.
//!
//! Provides `NativeCrossEncoderReranker<Id, R, S>` which implements `Reranker<Id>`.
//! Document texts are fetched by `RerankDocumentResolver<Id>` so the existing
//! `Reranker` trait (which only carries IDs and scores) does not need to change.

use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::{Result, RetrievalError};
use crate::hybrid::searcher::Reranker;

/// Native BERT cross-encoder returning sigmoid probabilities, not raw logits.
pub use lattice_inference::CrossEncoderModel as NativeCrossEncoderModel;

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
}

impl CrossEncoderScorer for NativeCrossEncoderModel {
    fn score_batch(&self, query: &str, documents: &[&str]) -> Vec<f32> {
        NativeCrossEncoderModel::score_batch(self, query, documents)
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
        if top_k == 0 || results.is_empty() {
            return Ok(Vec::new());
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
            return Ok(Vec::new());
        }

        let document_refs: Vec<&str> = retained.iter().map(|(_, _, text)| text.as_str()).collect();
        let scores = self.model.score_batch(query, &document_refs);
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

        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(_, id, score)| (id, DeterministicScore::from_f64(score as f64)))
            .collect())
    }
}

#[cfg(test)]
#[path = "cross_encoder/tests.rs"]
mod tests;
