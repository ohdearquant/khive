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
#[path = "search_config/tests.rs"]
mod tests;
