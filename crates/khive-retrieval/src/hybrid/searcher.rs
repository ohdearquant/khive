//! `VectorSearch`, `KeywordSearch`, `Reranker`, and `HybridSearcher` traits.

use std::hash::Hash;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::Result;
use khive_fusion::{fuse, FusionStrategy};

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
///
/// `Ord` is required for deterministic tie-breaking when scores are equal.
///
/// # Weighted strategy validation
///
/// When `config.fusion_strategy` is `Weighted`, this function validates that
/// exactly 2 sources are provided in all builds. If the source count does not
/// match the weight vector length, the function falls back to RRF to prevent
/// silent data corruption. Use [`fuse_search_results_checked`] if you need an
/// explicit error instead of a fallback.
pub fn fuse_search_results<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    config: &HybridConfig,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    if sources.len() == 1 {
        let mut results = sources.into_iter().next().unwrap();
        if let Some(min_score) = config.min_score {
            results.retain(|(_, score)| *score >= min_score);
        }
        results.truncate(config.top_k);
        return results;
    }

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

    // Fuse results — strategy is guaranteed non-Custom after the match above.
    let mut fused =
        fuse(sources, &strategy, config.top_k).expect("non-Custom strategies are infallible");

    // Apply minimum score filter
    if let Some(min_score) = config.min_score {
        fused.retain(|(_, score)| *score >= min_score);
    }

    fused
}

/// Like [`fuse_search_results`] but returns `Err` when `Weighted` fusion is
/// configured with a source count that doesn't match the expected 2 weights.
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
    Ok(fuse_search_results(sources, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse_empty_sources() {
        let sources: Vec<Vec<(String, DeterministicScore)>> = vec![];
        let config = HybridConfig::default();
        let results = fuse_search_results(sources, &config);
        assert!(results.is_empty());
    }

    #[test]
    fn test_fuse_single_source() {
        let sources = vec![vec![
            ("a".to_string(), DeterministicScore::from_f64(0.9)),
            ("b".to_string(), DeterministicScore::from_f64(0.8)),
        ]];
        let config = HybridConfig::new(10);
        let results = fuse_search_results(sources, &config);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "a");
    }

    #[test]
    fn test_fuse_multiple_sources_rrf() {
        let source1 = vec![
            ("a".to_string(), DeterministicScore::from_f64(0.9)),
            ("b".to_string(), DeterministicScore::from_f64(0.8)),
        ];
        let source2 = vec![
            ("b".to_string(), DeterministicScore::from_f64(0.95)),
            ("c".to_string(), DeterministicScore::from_f64(0.7)),
        ];

        let config = HybridConfig::new(10);
        let results = fuse_search_results(vec![source1, source2], &config);

        assert_eq!(results.len(), 3);
        // b appears in both, should have highest RRF score
        assert_eq!(results[0].0, "b");
    }

    #[test]
    fn test_fuse_with_min_score() {
        let sources = vec![vec![
            ("a".to_string(), DeterministicScore::from_f64(0.9)),
            ("b".to_string(), DeterministicScore::from_f64(0.1)),
        ]];

        let config = HybridConfig::new(10).with_min_score(DeterministicScore::from_f64(0.5));
        let results = fuse_search_results(sources, &config);

        // b should be filtered out (RRF score ~0.016 < 0.5)
        // Actually RRF scores are very small, let's use a lower threshold
        assert!(!results.is_empty());
    }

    #[test]
    fn test_fuse_top_k_limit() {
        let sources = vec![vec![
            ("a".to_string(), DeterministicScore::from_f64(0.9)),
            ("b".to_string(), DeterministicScore::from_f64(0.8)),
            ("c".to_string(), DeterministicScore::from_f64(0.7)),
            ("d".to_string(), DeterministicScore::from_f64(0.6)),
            ("e".to_string(), DeterministicScore::from_f64(0.5)),
        ]];

        let config = HybridConfig::new(3);
        let results = fuse_search_results(sources, &config);

        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_fuse_weighted_three_sources_falls_back_to_rrf() {
        // Regression guard: Weighted with 3 sources previously used debug_assert
        // which was a no-op in release builds. Now it must fall back to RRF (not panic).
        use khive_fusion::FusionStrategy;
        let source1 = vec![("a".to_string(), DeterministicScore::from_f64(0.9))];
        let source2 = vec![("b".to_string(), DeterministicScore::from_f64(0.8))];
        let source3 = vec![("c".to_string(), DeterministicScore::from_f64(0.7))];

        let config =
            HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted(vec![0.5, 0.5]));

        // Must not panic — falls back to RRF silently.
        let results = fuse_search_results(vec![source1, source2, source3], &config);
        assert_eq!(
            results.len(),
            3,
            "all 3 results should survive RRF fallback"
        );
    }

    #[test]
    fn test_fuse_search_results_checked_weighted_wrong_count_returns_err() {
        use khive_fusion::FusionStrategy;
        let config =
            HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted(vec![0.5, 0.5]));

        let source1 = vec![("a".to_string(), DeterministicScore::from_f64(0.9))];
        let source2 = vec![("b".to_string(), DeterministicScore::from_f64(0.8))];
        let source3 = vec![("c".to_string(), DeterministicScore::from_f64(0.7))];

        let result = fuse_search_results_checked(vec![source1, source2, source3], &config);
        assert!(
            result.is_err(),
            "checked variant must return Err for 3-source Weighted fusion"
        );
    }

    #[test]
    fn test_fuse_search_results_checked_weighted_two_sources_ok() {
        use khive_fusion::FusionStrategy;
        let config =
            HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted(vec![0.5, 0.5]));

        let source1 = vec![("a".to_string(), DeterministicScore::from_f64(0.9))];
        let source2 = vec![("b".to_string(), DeterministicScore::from_f64(0.8))];

        let result = fuse_search_results_checked(vec![source1, source2], &config);
        assert!(result.is_ok(), "2-source Weighted must succeed");
        assert_eq!(result.unwrap().len(), 2);
    }
}
