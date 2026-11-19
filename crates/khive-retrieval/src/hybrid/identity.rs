//! Passthrough baseline reranker.

use std::marker::PhantomData;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::Result;
use crate::hybrid::searcher::Reranker;

/// Baseline/passthrough reranker for pipelines and evaluation harnesses.
///
/// `rerank` performs no scoring: it returns `results` in their original
/// input order, truncated to `top_k`. Deterministic across calls — use this
/// as the control arm when comparing a real reranker's effect on ranking
/// quality, or as a no-op default while a model-backed reranker is unavailable.
#[derive(Debug, Default, Clone, Copy)]
pub struct IdentityReranker<Id> {
    _id: PhantomData<fn() -> Id>,
}

impl<Id> IdentityReranker<Id> {
    /// Construct a new `IdentityReranker`.
    pub fn new() -> Self {
        Self { _id: PhantomData }
    }
}

#[async_trait]
impl<Id: Send + Sync + 'static> Reranker<Id> for IdentityReranker<Id> {
    async fn rerank(
        &self,
        _query: &str,
        results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
    ) -> Result<Vec<(Id, DeterministicScore)>> {
        Ok(results.into_iter().take(top_k).collect())
    }
}

#[cfg(test)]
#[path = "identity/tests.rs"]
mod tests;
