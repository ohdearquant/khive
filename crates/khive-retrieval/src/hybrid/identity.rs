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
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_original_order() {
        let reranker = IdentityReranker::new();
        let results = vec![
            (1u32, DeterministicScore::from_f64(0.1)),
            (2u32, DeterministicScore::from_f64(0.9)),
            (3u32, DeterministicScore::from_f64(0.5)),
        ];
        let out = reranker.rerank("q", results.clone(), 3).await.unwrap();
        assert_eq!(out, results);
    }

    #[tokio::test]
    async fn truncates_to_top_k() {
        let reranker = IdentityReranker::new();
        let results = vec![
            (1u32, DeterministicScore::from_f64(0.1)),
            (2u32, DeterministicScore::from_f64(0.9)),
            (3u32, DeterministicScore::from_f64(0.5)),
        ];
        let out = reranker.rerank("q", results.clone(), 2).await.unwrap();
        assert_eq!(out, results[..2]);
    }

    #[tokio::test]
    async fn top_k_zero_returns_empty() {
        let reranker = IdentityReranker::new();
        let results = vec![(1u32, DeterministicScore::from_f64(0.1))];
        let out = reranker.rerank("q", results, 0).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn top_k_larger_than_len_returns_all() {
        let reranker = IdentityReranker::new();
        let results = vec![
            (1u32, DeterministicScore::from_f64(0.1)),
            (2u32, DeterministicScore::from_f64(0.9)),
        ];
        let out = reranker.rerank("q", results, 10).await.unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn deterministic_across_calls() {
        let reranker = IdentityReranker::new();
        let results = vec![
            (1u32, DeterministicScore::from_f64(0.1)),
            (2u32, DeterministicScore::from_f64(0.9)),
            (3u32, DeterministicScore::from_f64(0.5)),
        ];
        let out1 = reranker.rerank("q", results.clone(), 3).await.unwrap();
        let out2 = reranker.rerank("q", results, 3).await.unwrap();
        let ids1: Vec<_> = out1.iter().map(|(id, _)| *id).collect();
        let ids2: Vec<_> = out2.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids1, ids2);
    }
}
