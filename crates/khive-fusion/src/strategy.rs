//! Fusion strategy types.

use serde::{Deserialize, Serialize};

/// Default RRF constant k=60, commonly used after Cormack, Clarke,
/// and Büttcher (SIGIR 2009).
pub const DEFAULT_RRF_K: usize = 60;

/// Fusion strategy for combining ranked result lists.
///
/// See module-level docs for algorithm details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionStrategy {
    /// Reciprocal Rank Fusion (default, recommended).
    ///
    /// Uses only ranks, making it robust to different score distributions.
    /// Formula: score(d) = Σ 1/(k + rank_i(d))
    #[serde(alias = "Rrf")]
    Rrf {
        /// Smoothing constant. Higher values reduce impact of rank differences.
        /// Default: 60 (standard in literature).
        k: usize,
    },

    /// Weighted linear combination of scores.
    ///
    /// Requires score normalization for different score scales (e.g., vector
    /// similarity 0-1 vs BM25 0-∞).
    ///
    /// Weights are normalized to sum to 1.0 internally.
    #[serde(alias = "Weighted")]
    Weighted {
        /// Weights for each source (will be normalized).
        weights: Vec<f64>,
    },

    /// Take union with max score per ID.
    ///
    /// Useful when you want the best score from any source.
    #[serde(alias = "Union")]
    Union,

    /// Skip BM25 entirely — return only vector (HNSW) results.
    ///
    /// Use when keyword search degrades quality (short queries, code search).
    /// The result list is the raw HNSW output with no fusion step.
    #[serde(alias = "VectorOnly")]
    VectorOnly,

    /// Skip HNSW entirely — return only BM25 keyword results.
    ///
    /// Use for exact-match retrieval (medication names, identifiers, slugs).
    /// The result list is the raw BM25 output with no fusion step.
    #[serde(alias = "KeywordOnly")]
    KeywordOnly,
}

impl Default for FusionStrategy {
    fn default() -> Self {
        Self::Rrf { k: DEFAULT_RRF_K }
    }
}

impl FusionStrategy {
    /// Create an RRF strategy with default k=60.
    #[inline]
    pub fn rrf() -> Self {
        Self::Rrf { k: DEFAULT_RRF_K }
    }

    /// Create an RRF strategy with custom k value.
    #[inline]
    pub fn rrf_with_k(k: usize) -> Self {
        Self::Rrf { k: k.max(1) } // Ensure k >= 1
    }

    /// Create a weighted strategy with given weights.
    #[inline]
    pub fn weighted(weights: Vec<f64>) -> Self {
        Self::Weighted { weights }
    }

    /// Create a union strategy.
    #[inline]
    pub fn union() -> Self {
        Self::Union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fusion_strategy_default() {
        let default = FusionStrategy::default();
        assert_eq!(default, FusionStrategy::Rrf { k: 60 });
    }

    #[test]
    fn test_fusion_strategy_builders() {
        assert_eq!(FusionStrategy::rrf(), FusionStrategy::Rrf { k: 60 });
        assert_eq!(
            FusionStrategy::rrf_with_k(20),
            FusionStrategy::Rrf { k: 20 }
        );
        assert_eq!(FusionStrategy::rrf_with_k(0), FusionStrategy::Rrf { k: 1 }); // min enforced
        assert_eq!(
            FusionStrategy::weighted(vec![0.5, 0.5]),
            FusionStrategy::Weighted {
                weights: vec![0.5, 0.5]
            }
        );
        assert_eq!(FusionStrategy::union(), FusionStrategy::Union);
    }
}
