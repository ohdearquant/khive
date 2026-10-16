//! `VectorSearch`, `KeywordSearch`, `Reranker`, and `HybridSearcher` traits.

use std::hash::Hash;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::Result;
use khive_fusion::{fuse, FuseError, FusionStrategy};

use super::config::{HybridConfig, Query};

/// Trait for vector similarity search (HNSW, flat scan, IVF).
#[async_trait]
pub trait VectorSearch: Send + Sync {
    /// Identifier type; `Ord` required for deterministic tie-breaking.
    type Id: Eq + Hash + Clone + Ord + Send + Sync;

    /// Perform vector-only search. Returns `(Id, score)` pairs sorted descending.
    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<(Self::Id, DeterministicScore)>>;
}

/// Trait for keyword-based search (BM25, TF-IDF).
#[async_trait]
pub trait KeywordSearch: Send + Sync {
    /// Identifier type; `Ord` required for deterministic tie-breaking.
    type Id: Eq + Hash + Clone + Ord + Send + Sync;

    /// Perform keyword-only search. Returns `(Id, score)` pairs sorted descending.
    async fn keyword_search(
        &self,
        text: &str,
        top_k: usize,
    ) -> Result<Vec<(Self::Id, DeterministicScore)>>;
}

/// Combines [`VectorSearch`] and [`KeywordSearch`] (same `Id` type) with configurable fusion.
#[async_trait]
pub trait HybridSearcher: VectorSearch + KeywordSearch<Id = <Self as VectorSearch>::Id> {
    /// Perform hybrid search. Returns `(Id, score)` sorted by fused score descending.
    async fn hybrid_search(
        &self,
        query: &Query,
        config: &HybridConfig,
    ) -> Result<Vec<(<Self as VectorSearch>::Id, DeterministicScore)>>;
}

/// Reranking trait: cross-encoder, LLM-based, or custom scoring over pre-ranked results.
#[async_trait]
pub trait Reranker<Id: Send + Sync + 'static>: Send + Sync {
    /// Rerank `results` using `query` context. Returns top `top_k` pairs.
    async fn rerank(
        &self,
        query: &str,
        results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
    ) -> Result<Vec<(Id, DeterministicScore)>>;
}

/// Helper function to perform fusion on search results.
///
/// This can be used by implementors of [`HybridSearcher`] to fuse results
/// from their [`VectorSearch`] and [`KeywordSearch`] implementations.
/// Two-arm vector/text callers use the positional order `[vector, keyword]`.
/// Keep empty arms in that order so positional weighted strategies cannot
/// rebind a keyword-only result set to the vector weight (or vice versa).
/// Generic RRF and Union callers may supply N sources in a caller-defined,
/// documented order.
///
/// `Ord` is required for deterministic tie-breaking when scores are equal.
///
/// # Weighted strategy validation
///
/// When `config.fusion_strategy` is `Weighted`, this function validates that
/// exactly 2 vector/text source slots are provided in all builds. A missing
/// arm is represented by an empty slot; any other source count falls back to
/// RRF to prevent silent positional rebinding. Use
/// [`fuse_search_results_checked`] if you need an explicit error instead.
///
/// `WeightedRrf` never degrades to another strategy here: when its parameters,
/// weight count, or score range are invalid, this helper returns [`FuseError`],
/// including when all supplied source slots are empty.
pub fn fuse_search_results<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    config: &HybridConfig,
) -> std::result::Result<Vec<(Id, DeterministicScore)>, FuseError> {
    // Determine fusion strategy — Custom falls back to RRF (same as Weighted
    // mismatch).  Callers that need a hard error use fuse_search_results_checked.
    let strategy = match &config.fusion_strategy {
        FusionStrategy::Weighted { .. } => {
            if sources.len() != 2 {
                FusionStrategy::rrf()
            } else {
                let (v, k) = config.normalized_weights();
                FusionStrategy::weighted(vec![v, k])
            }
        }
        FusionStrategy::Custom { .. } => FusionStrategy::rrf(),
        other => other.clone(),
    };

    let mut fused = fuse(sources, &strategy, config.top_k)?;

    // Apply minimum score filter
    if let Some(min_score) = config.min_score {
        fused.retain(|(_, score)| *score >= min_score);
    }

    Ok(fused)
}

/// Like [`fuse_search_results`] but returns `Err` for invalid weighted source
/// shapes, weighted-RRF parameters, or weighted-RRF score overflow.
///
/// Use this in code paths that should not silently fall back to RRF.
pub fn fuse_search_results_checked<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    config: &HybridConfig,
) -> Result<Vec<(Id, DeterministicScore)>> {
    match &config.fusion_strategy {
        FusionStrategy::Custom { name, .. } => {
            return Err(crate::error::RetrievalError::Fusion(format!(
                "Custom strategy {name:?} requires runtime dispatch"
            )));
        }
        FusionStrategy::Weighted { .. } if sources.len() != 2 => {
            return Err(crate::error::RetrievalError::Fusion(format!(
                "Weighted fusion requires exactly 2 sources, got {}",
                sources.len()
            )));
        }
        _ => {}
    }
    fuse_search_results(sources, config)
        .map_err(|error| crate::error::RetrievalError::Fusion(error.to_string()))
}

#[cfg(test)]
#[path = "searcher/tests.rs"]
mod tests;
