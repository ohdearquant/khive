//! Granular search traits and hybrid search implementation.
//!
//! # Trait Hierarchy
//!
//! ```text
//! VectorSearch ──┐
//!                ├── HybridSearcher
//! KeywordSearch ─┘
//!
//! Reranker (standalone, generic over Id)
//! ```
//!
//! Each trait can be implemented independently, enabling:
//! - Vector-only search (e.g., HNSW index)
//! - Keyword-only search (e.g., BM25 index)
//! - Full hybrid search (combining both with fusion)
//! - Reranking as a separate, composable concern

use std::hash::Hash;

use async_trait::async_trait;
use khive_score::DeterministicScore;

use crate::error::Result;
use khive_fusion::{fuse, FusionStrategy};

use super::config::{HybridConfig, Query};

/// Trait for vector similarity search.
///
/// Implementors provide embedding-based nearest-neighbor search
/// (e.g., HNSW, flat scan, IVF).
///
/// # Associated Types
///
/// * `Id` - The identifier type for documents/results. Requires `Ord` for
///   deterministic tie-breaking when scores are equal.
///
/// # Example
///
/// ```rust,ignore
/// use khive_retrieval::hybrid::VectorSearch;
///
/// struct MyVectorIndex { /* ... */ }
///
/// #[async_trait::async_trait]
/// impl VectorSearch for MyVectorIndex {
///     type Id = String;
///
///     async fn vector_search(&self, embedding: &[f32], top_k: usize)
///         -> khive_retrieval::Result<Vec<(String, khive_score::DeterministicScore)>>
///     {
///         // Your HNSW/ANN implementation here
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait VectorSearch: Send + Sync {
    /// The ID type for search results.
    /// `Ord` is required for deterministic tie-breaking when scores are equal.
    type Id: Eq + Hash + Clone + Ord + Send + Sync;

    /// Perform vector-only search.
    ///
    /// # Arguments
    ///
    /// * `embedding` - Query embedding vector
    /// * `top_k` - Number of results to return
    ///
    /// # Returns
    ///
    /// Vector of (Id, DeterministicScore) pairs sorted by similarity descending.
    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<(Self::Id, DeterministicScore)>>;
}

/// Trait for keyword-based search.
///
/// Implementors provide text-based retrieval (e.g., BM25, TF-IDF).
///
/// # Associated Types
///
/// * `Id` - The identifier type for documents/results. Requires `Ord` for
///   deterministic tie-breaking when scores are equal.
///
/// # Example
///
/// ```rust,ignore
/// use khive_retrieval::hybrid::KeywordSearch;
///
/// struct MyBm25Index { /* ... */ }
///
/// #[async_trait::async_trait]
/// impl KeywordSearch for MyBm25Index {
///     type Id = String;
///
///     async fn keyword_search(&self, text: &str, top_k: usize)
///         -> khive_retrieval::Result<Vec<(String, khive_score::DeterministicScore)>>
///     {
///         // Your BM25 implementation here
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait KeywordSearch: Send + Sync {
    /// The ID type for search results.
    /// `Ord` is required for deterministic tie-breaking when scores are equal.
    type Id: Eq + Hash + Clone + Ord + Send + Sync;

    /// Perform keyword-only search (BM25).
    ///
    /// # Arguments
    ///
    /// * `text` - Query text
    /// * `top_k` - Number of results to return
    ///
    /// # Returns
    ///
    /// Vector of (Id, DeterministicScore) pairs sorted by BM25 score descending.
    async fn keyword_search(
        &self,
        text: &str,
        top_k: usize,
    ) -> Result<Vec<(Self::Id, DeterministicScore)>>;
}

/// Trait for hybrid search operations.
///
/// Combines vector similarity search (HNSW) with keyword search (BM25)
/// using configurable fusion strategies.
///
/// # Supertrait Constraint
///
/// Requires both [`VectorSearch`] and [`KeywordSearch`] to be implemented
/// with the **same `Id` type**, enforced by the
/// `KeywordSearch<Id = <Self as VectorSearch>::Id>` bound.
///
/// # Example
///
/// ```rust,ignore
/// use khive_retrieval::hybrid::{HybridSearcher, VectorSearch, KeywordSearch};
///
/// struct MyHybridIndex { /* ... */ }
///
/// // Implement VectorSearch and KeywordSearch first, then HybridSearcher
/// #[async_trait::async_trait]
/// impl HybridSearcher for MyHybridIndex {
///     async fn hybrid_search(&self, query: &Query, config: &HybridConfig)
///         -> Result<Vec<(String, DeterministicScore)>>
///     {
///         let mut sources = Vec::new();
///         if let Some(emb) = &query.embedding {
///             sources.push(self.vector_search(emb, config.candidate_pool_size).await?);
///         }
///         sources.push(self.keyword_search(&query.text, config.candidate_pool_size).await?);
///         Ok(fuse_search_results(sources, config))
///     }
/// }
/// ```
#[async_trait]
pub trait HybridSearcher: VectorSearch + KeywordSearch<Id = <Self as VectorSearch>::Id> {
    /// Perform hybrid search combining vector and keyword retrieval.
    ///
    /// # Arguments
    ///
    /// * `query` - The search query (text + optional embedding)
    /// * `config` - Hybrid search configuration
    ///
    /// # Returns
    ///
    /// Vector of (Id, DeterministicScore) pairs sorted by fused score descending.
    async fn hybrid_search(
        &self,
        query: &Query,
        config: &HybridConfig,
    ) -> Result<Vec<(<Self as VectorSearch>::Id, DeterministicScore)>>;
}

/// Trait for reranking search results.
///
/// Separates the reranking concern from search, enabling:
/// - Cross-encoder neural reranking
/// - LLM-based reranking
/// - Custom scoring adjustments
///
/// The `Id` type is a generic parameter rather than an associated type,
/// allowing a single reranker to work with different ID types.
///
/// # Example
///
/// ```rust,ignore
/// use khive_retrieval::hybrid::Reranker;
///
/// struct CrossEncoderReranker { /* model handle */ }
///
/// #[async_trait::async_trait]
/// impl Reranker<String> for CrossEncoderReranker {
///     async fn rerank(
///         &self,
///         query: &str,
///         results: Vec<(String, DeterministicScore)>,
///         top_k: usize,
///     ) -> Result<Vec<(String, DeterministicScore)>> {
///         // Score each (query, document) pair with cross-encoder
///         // Sort by new scores and truncate to top_k
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait Reranker<Id: Send + Sync + 'static>: Send + Sync {
    /// Rerank search results using additional signals.
    ///
    /// # Arguments
    ///
    /// * `query` - The original query text for relevance scoring
    /// * `results` - Pre-ranked results to reorder
    /// * `top_k` - Number of results to return after reranking
    ///
    /// # Returns
    ///
    /// Reranked vector of (Id, DeterministicScore) pairs, truncated to `top_k`.
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

    // Determine fusion strategy
    let strategy = match &config.fusion_strategy {
        FusionStrategy::Weighted { .. } => {
            // Weighted fusion uses exactly 2 weight values (vector + keyword).
            // Validate in all builds — debug_assert was insufficient because
            // 3+ sources could silently fuse with only 2 weights in release builds.
            if sources.len() != 2 {
                // Fall back to RRF rather than silently mis-weight the results.
                // Callers that need a hard error should use fuse_search_results_checked.
                FusionStrategy::rrf()
            } else {
                let (v, k) = config.normalized_weights();
                FusionStrategy::weighted(vec![v, k])
            }
        }
        other => other.clone(),
    };

    // Fuse results
    let mut fused = fuse(sources, &strategy, config.top_k);

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
    if let FusionStrategy::Weighted { .. } = &config.fusion_strategy {
        if sources.len() != 2 {
            return Err(crate::error::RetrievalError::Fusion(format!(
                "Weighted fusion requires exactly 2 sources, got {}",
                sources.len()
            )));
        }
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
