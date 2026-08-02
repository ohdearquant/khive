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
pub fn fuse_search_results<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    config: &HybridConfig,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
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
/// configured without exactly two vector/text source slots.
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
        assert_eq!(results[0].1, khive_score::rrf_score(1, 60));
        assert_eq!(results[1].1, khive_score::rrf_score(2, 60));
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
    fn weighted_sources_use_vector_then_keyword_order() {
        let vector = vec![("vector".to_string(), DeterministicScore::from_f64(0.9))];
        let keyword = vec![("keyword".to_string(), DeterministicScore::from_f64(0.9))];
        let config = HybridConfig::new(10)
            .with_fusion_strategy(FusionStrategy::weighted(vec![0.7, 0.3]))
            .with_weights(0.7, 0.3);

        let results = fuse_search_results(vec![vector, keyword], &config);

        assert_eq!(results[0].0, "vector");
        assert!(results[0].1 > results[1].1);
    }

    #[test]
    fn test_fuse_with_min_score() {
        let sources = vec![vec![
            ("a".to_string(), DeterministicScore::from_f64(0.9)),
            ("b".to_string(), DeterministicScore::from_f64(0.1)),
        ]];

        let config = HybridConfig::new(10).with_min_score(DeterministicScore::from_f64(0.5));
        let results = fuse_search_results(sources, &config);

        assert!(
            results.is_empty(),
            "the one-arm RRF transform must run before the fused-domain score floor"
        );
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

    #[test]
    fn test_fuse_search_results_checked_weighted_empty_arm_keeps_slot() {
        let config = HybridConfig::new(10)
            .with_fusion_strategy(FusionStrategy::weighted(vec![0.7, 0.3]))
            .with_weights(0.7, 0.3);
        let keyword = vec![("keyword".to_string(), DeterministicScore::from_f64(0.8))];

        let result = fuse_search_results_checked(vec![Vec::new(), keyword], &config)
            .expect("an empty vector arm still occupies its canonical slot");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "keyword");
        assert!((result[0].1.to_f64() - 0.3).abs() < 1e-9);
    }
}
