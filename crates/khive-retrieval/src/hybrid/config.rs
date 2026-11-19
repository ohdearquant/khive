//! Hybrid search configuration types.

use std::time::Duration;

use khive_score::DeterministicScore;
use serde::{Deserialize, Serialize};

use khive_fusion::FusionStrategy;

/// Default candidate pool multiplier over top_k.
pub const DEFAULT_POOL_MULTIPLIER: usize = 5;

/// Query for hybrid search.
///
/// Combines text for keyword search and optional embedding for vector search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    /// Text for keyword search (required).
    pub text: String,

    /// Pre-computed embedding for vector search (optional).
    ///
    /// If None, vector search is skipped or caller must provide.
    pub embedding: Option<Vec<f32>>,

    /// Optional filters to apply post-retrieval.
    pub filters: Option<serde_json::Value>,
}

impl Query {
    /// Create a new query with text only (keyword search).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            embedding: None,
            filters: None,
        }
    }

    /// Create a query with both text and embedding (hybrid search).
    pub fn hybrid(text: impl Into<String>, embedding: Vec<f32>) -> Self {
        Self {
            text: text.into(),
            embedding: Some(embedding),
            filters: None,
        }
    }

    /// Add filters to the query.
    #[must_use]
    pub fn with_filters(mut self, filters: serde_json::Value) -> Self {
        self.filters = Some(filters);
        self
    }

    /// Check if this query supports vector search.
    pub fn has_embedding(&self) -> bool {
        self.embedding.is_some()
    }
}

/// Raw wire format for [`HybridConfig`], used by `TryFrom` validation.
#[derive(Deserialize)]
struct RawHybridConfig {
    fusion_strategy: FusionStrategy,
    top_k: usize,
    candidate_pool_size: usize,
    min_score: Option<DeterministicScore>,
    vector_weight: f64,
    keyword_weight: f64,
    #[serde(default, with = "crate::timeout::serde_opt_duration")]
    timeout: Option<Duration>,
}

impl TryFrom<RawHybridConfig> for HybridConfig {
    type Error = String;

    fn try_from(raw: RawHybridConfig) -> Result<Self, Self::Error> {
        if !raw.vector_weight.is_finite() {
            return Err(format!(
                "vector_weight must be finite, got {}",
                raw.vector_weight
            ));
        }
        if !raw.keyword_weight.is_finite() {
            return Err(format!(
                "keyword_weight must be finite, got {}",
                raw.keyword_weight
            ));
        }
        Ok(HybridConfig {
            fusion_strategy: raw.fusion_strategy,
            top_k: raw.top_k,
            candidate_pool_size: raw.candidate_pool_size,
            min_score: raw.min_score,
            vector_weight: raw.vector_weight,
            keyword_weight: raw.keyword_weight,
            timeout: raw.timeout,
        })
    }
}

/// Configuration for hybrid search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawHybridConfig")]
pub struct HybridConfig {
    /// Fusion strategy to use (default: RRF with k=60).
    pub fusion_strategy: FusionStrategy,

    /// Number of results to return.
    pub top_k: usize,

    /// Candidates to fetch from each retriever before fusion.
    ///
    /// Should be >= 5 * top_k for quality fusion.
    pub candidate_pool_size: usize,

    /// Minimum score threshold (post-fusion).
    pub min_score: Option<DeterministicScore>,

    /// Weight for vector search results (0.0 to 1.0).
    ///
    /// Only used when fusion_strategy is Weighted.
    pub vector_weight: f64,

    /// Weight for keyword search results (0.0 to 1.0).
    ///
    /// Only used when fusion_strategy is Weighted.
    pub keyword_weight: f64,

    /// Optional timeout for the entire search operation.
    ///
    /// If set, the search will be cancelled if it exceeds this duration,
    /// returning [`crate::error::RetrievalError::QueryTimeout`].
    /// If None, no timeout is applied.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::timeout::serde_opt_duration"
    )]
    pub timeout: Option<Duration>,
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self {
            fusion_strategy: FusionStrategy::rrf(),
            top_k: 10,
            candidate_pool_size: 50, // 5 * top_k
            min_score: None,
            vector_weight: 0.7,
            keyword_weight: 0.3,
            timeout: None,
        }
    }
}

impl HybridConfig {
    /// Create a new config with specified top_k.
    ///
    /// The candidate pool size is `top_k * DEFAULT_POOL_MULTIPLIER`, saturating
    /// at `usize::MAX` on overflow (rather than wrapping or panicking in debug).
    pub fn new(top_k: usize) -> Self {
        Self {
            top_k,
            candidate_pool_size: top_k.saturating_mul(DEFAULT_POOL_MULTIPLIER),
            ..Default::default()
        }
    }

    /// Set the fusion strategy.
    #[must_use]
    pub fn with_fusion_strategy(mut self, strategy: FusionStrategy) -> Self {
        self.fusion_strategy = strategy;
        self
    }

    /// Set the candidate pool size.
    #[must_use]
    pub fn with_pool_size(mut self, size: usize) -> Self {
        self.candidate_pool_size = size;
        self
    }

    /// Set the minimum score threshold.
    #[must_use]
    pub fn with_min_score(mut self, score: DeterministicScore) -> Self {
        self.min_score = Some(score);
        self
    }

    /// Set weights for weighted fusion (clamped to [0.0, 1.0]). Debug-asserts both weights are finite.
    #[must_use]
    pub fn with_weights(mut self, vector: f64, keyword: f64) -> Self {
        debug_assert!(
            vector.is_finite(),
            "vector weight must be finite, got {vector}"
        );
        debug_assert!(
            keyword.is_finite(),
            "keyword weight must be finite, got {keyword}"
        );
        self.vector_weight = vector.clamp(0.0, 1.0);
        self.keyword_weight = keyword.clamp(0.0, 1.0);
        self
    }

    /// Set the search timeout.
    ///
    /// If the search operation exceeds this duration, it will return
    /// [`crate::error::RetrievalError::QueryTimeout`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Get normalized weights that sum to 1.0.
    ///
    /// If both weights are zero or their sum is non-finite, returns equal weights (0.5, 0.5).
    pub fn normalized_weights(&self) -> (f64, f64) {
        let sum = self.vector_weight + self.keyword_weight;
        if sum <= 0.0 || !sum.is_finite() {
            (0.5, 0.5)
        } else {
            (self.vector_weight / sum, self.keyword_weight / sum)
        }
    }
}

#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
