//! Per-call hybrid search configuration (fusion strategy + top_k). Default: RRF k=60, top_k=10.

use serde::{Deserialize, Serialize};

use khive_fusion::{FusionStrategy, DEFAULT_RRF_K};

/// Per-call configuration for hybrid search retrieval and fusion.
///
/// Added to `RecallOptions` and `ComposeOptions` as `search: Option<SearchConfig>`.
/// When `None`, callers receive identical behavior to pre-Phase-7 code.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Maximum number of results to return.
    ///
    /// Default: 10.
    #[serde(default = "default_top_k")]
    pub top_k: usize,

    /// Candidate pool multiplier over `top_k`.
    ///
    /// The retriever fetches `top_k * candidate_pool_multiplier` candidates
    /// before fusion and reranking. Higher values improve recall quality at
    /// the cost of more computation.
    ///
    /// Default: 3.
    #[serde(default = "default_multiplier")]
    pub candidate_pool_multiplier: usize,

    /// Fusion strategy for combining vector and keyword result lists.
    ///
    /// Default: RRF with k=60.
    #[serde(default = "default_fusion")]
    pub fusion_strategy: FusionStrategy,

    /// Weight for vector search in weighted fusion (0.0 to 1.0).
    ///
    /// Only used when `fusion_strategy` is `Weighted`. Keyword weight is
    /// implicitly `1.0 - vector_weight`.
    ///
    /// Default: 0.5 (balanced).
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,

    /// Minimum score threshold.
    ///
    /// Results with a final score below this value are filtered out.
    /// When `None`, no threshold is applied.
    ///
    /// Default: None.
    #[serde(default)]
    pub min_score: Option<f64>,
}

fn default_top_k() -> usize {
    10
}

fn default_multiplier() -> usize {
    3
}

fn default_fusion() -> FusionStrategy {
    FusionStrategy::Rrf { k: DEFAULT_RRF_K }
}

fn default_vector_weight() -> f64 {
    0.5
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            top_k: default_top_k(),
            candidate_pool_multiplier: default_multiplier(),
            fusion_strategy: default_fusion(),
            vector_weight: default_vector_weight(),
            min_score: None,
        }
    }
}

impl SearchConfig {
    /// Preset: skip BM25 entirely, return only vector search results.
    ///
    /// Use when keyword search degrades quality (e.g., short queries, code search).
    pub fn vector_only() -> Self {
        Self {
            top_k: default_top_k(),
            candidate_pool_multiplier: 1,
            fusion_strategy: FusionStrategy::VectorOnly,
            vector_weight: 1.0,
            min_score: None,
        }
    }

    /// Preset: skip HNSW entirely, return only BM25 keyword results.
    ///
    /// Use for exact-match retrieval (e.g., medication names, identifiers).
    pub fn keyword_only() -> Self {
        Self {
            top_k: default_top_k(),
            candidate_pool_multiplier: 1,
            fusion_strategy: FusionStrategy::KeywordOnly,
            vector_weight: 0.0,
            min_score: None,
        }
    }

    /// Preset: balanced hybrid search using RRF with k=60.
    ///
    /// Equivalent to `SearchConfig::default()`. Combines vector and keyword
    /// results with equal weight using Reciprocal Rank Fusion.
    pub fn hybrid_balanced() -> Self {
        Self::default()
    }

    /// Set a custom top_k.
    #[must_use]
    pub fn with_top_k(mut self, top_k: usize) -> Self {
        self.top_k = top_k;
        self
    }

    /// Set the candidate pool multiplier.
    #[must_use]
    pub fn with_candidate_pool_multiplier(mut self, multiplier: usize) -> Self {
        self.candidate_pool_multiplier = multiplier;
        self
    }

    /// Set a minimum score filter. Debug-asserts `min` is finite.
    #[must_use]
    pub fn with_min_score(mut self, min: f64) -> Self {
        debug_assert!(min.is_finite(), "min_score must be finite, got {min}");
        self.min_score = Some(min);
        self
    }

    /// Compute the candidate pool size from `top_k * candidate_pool_multiplier`.
    ///
    /// Uses saturating multiplication to avoid overflow on pathological inputs.
    pub fn candidate_pool_size(&self) -> usize {
        self.top_k
            .saturating_mul(self.candidate_pool_multiplier.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = SearchConfig::default();
        assert_eq!(cfg.top_k, 10);
        assert_eq!(cfg.candidate_pool_multiplier, 3);
        assert!((cfg.vector_weight - 0.5).abs() < f64::EPSILON);
        assert!(cfg.min_score.is_none());
        assert!(matches!(cfg.fusion_strategy, FusionStrategy::Rrf { k: 60 }));
    }

    #[test]
    fn test_vector_only_preset() {
        let cfg = SearchConfig::vector_only();
        assert!(matches!(cfg.fusion_strategy, FusionStrategy::VectorOnly));
        assert!((cfg.vector_weight - 1.0).abs() < f64::EPSILON);
        assert_eq!(cfg.candidate_pool_multiplier, 1);
    }

    #[test]
    fn test_keyword_only_preset() {
        let cfg = SearchConfig::keyword_only();
        assert!(matches!(cfg.fusion_strategy, FusionStrategy::KeywordOnly));
        assert!((cfg.vector_weight - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_hybrid_balanced_is_default() {
        let balanced = SearchConfig::hybrid_balanced();
        let default = SearchConfig::default();
        assert_eq!(balanced.top_k, default.top_k);
        assert_eq!(
            balanced.candidate_pool_multiplier,
            default.candidate_pool_multiplier
        );
        assert!((balanced.vector_weight - default.vector_weight).abs() < f64::EPSILON);
    }

    #[test]
    fn test_candidate_pool_size() {
        let cfg = SearchConfig::default();
        assert_eq!(cfg.candidate_pool_size(), 30); // 10 * 3

        let cfg = SearchConfig::vector_only().with_top_k(5);
        assert_eq!(cfg.candidate_pool_size(), 5); // 5 * 1
    }

    #[test]
    fn test_builder_methods() {
        let cfg = SearchConfig::default()
            .with_top_k(20)
            .with_candidate_pool_multiplier(5)
            .with_min_score(0.3);
        assert_eq!(cfg.top_k, 20);
        assert_eq!(cfg.candidate_pool_multiplier, 5);
        assert_eq!(cfg.min_score, Some(0.3));
        assert_eq!(cfg.candidate_pool_size(), 100);
    }

    #[test]
    fn test_serde_roundtrip() {
        let cfg = SearchConfig {
            top_k: 15,
            candidate_pool_multiplier: 4,
            fusion_strategy: FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
            vector_weight: 0.7,
            min_score: Some(0.1),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: SearchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.top_k, 15);
        assert_eq!(back.candidate_pool_multiplier, 4);
        assert_eq!(back.min_score, Some(0.1));
    }
}
