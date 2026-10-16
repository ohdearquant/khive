//! Native cross-encoder reranking via `khive-inference`.
//!
//! Provides `NativeCrossEncoderReranker<Id, R, S>` which implements `Reranker<Id>`.
//! Document texts are fetched by `RerankDocumentResolver<Id>` so the existing
//! `Reranker` trait (which only carries IDs and scores) does not need to change.

use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::{Result, RetrievalError};
use crate::hybrid::searcher::Reranker;

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
    /// is represented as `None`; the reranker will return an error in that case.
    async fn resolve_documents(&self, ids: &[Id]) -> Result<Vec<Option<String>>>;
}

/// Synchronous cross-encoder scorer abstraction (for testability).
pub trait CrossEncoderScorer: Send + Sync {
    /// Score a query against a batch of documents; returns one value per document.
    fn score_batch(&self, query: &str, documents: &[&str]) -> Vec<f32>;
}

// TODO(port-rerank): khive-inference not ported yet; CrossEncoderModel impl disabled.
// impl CrossEncoderScorer for khive_inference::CrossEncoderModel { ... }

/// Reranker that scores candidates with a native cross-encoder model.
///
/// The generic parameter `S` is the scorer implementation (defaults to no external dep
/// in this OSS build; use a concrete scorer by passing one explicitly).
/// Tests substitute a lightweight fake scorer.
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

// TODO(port-rerank): from_directory constructor requires khive-inference::CrossEncoderModel.
// Re-enable once khive-inference is ported.
// impl<Id, R> NativeCrossEncoderReranker<Id, R, khive_inference::CrossEncoderModel> { ... }

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

        let mut documents: Vec<String> = Vec::with_capacity(resolved.len());
        for (idx, opt) in resolved.into_iter().enumerate() {
            let text = opt.ok_or_else(|| {
                RetrievalError::rerank(format!(
                    "missing document text for rerank candidate at index {idx}"
                ))
            })?;
            documents.push(text);
        }

        let document_refs: Vec<&str> = documents.iter().map(String::as_str).collect();
        let scores = self.model.score_batch(query, &document_refs);
        if scores.len() != results.len() {
            return Err(RetrievalError::rerank(format!(
                "model returned {} scores for {} candidates",
                scores.len(),
                results.len()
            )));
        }

        let mut scored: Vec<(usize, Id, f32)> = results
            .into_iter()
            .zip(scores)
            .enumerate()
            .map(|(idx, ((id, _), score))| (idx, id, score))
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
