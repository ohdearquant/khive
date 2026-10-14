//! Fusion strategy types with invariant validation.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Default RRF constant k=60, standard in literature (Cormack et al., 2009).
pub const DEFAULT_RRF_K: usize = 60;

/// Error returned when a [`FusionStrategy`] fails invariant validation.
#[derive(Debug, Clone, PartialEq)]
pub enum FusionStrategyError {
    /// RRF k must be >= 1 to avoid division by zero.
    RrfKZero,
    /// Weighted strategy weights contain NaN.
    WeightNaN,
    /// Weighted strategy weights contain infinity.
    WeightInfinite,
    /// Weighted RRF requires every weight to be strictly positive.
    WeightNotPositive,
    /// Custom strategy name must not be empty.
    CustomNameEmpty,
}

impl fmt::Display for FusionStrategyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RrfKZero => write!(f, "Rrf k must be >= 1"),
            Self::WeightNaN => write!(f, "Weighted weights must not contain NaN"),
            Self::WeightInfinite => write!(f, "Weighted weights must not contain infinity"),
            Self::WeightNotPositive => write!(f, "WeightedRrf weights must be strictly positive"),
            Self::CustomNameEmpty => write!(f, "Custom strategy name must not be empty"),
        }
    }
}

impl std::error::Error for FusionStrategyError {}

/// Raw serde form used for deserialization before validation.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawFusionStrategy {
    #[serde(alias = "Rrf")]
    Rrf { k: usize },
    #[serde(alias = "Weighted")]
    Weighted { weights: Vec<f64> },
    #[serde(alias = "WeightedRrf")]
    WeightedRrf { k: usize, weights: Vec<f64> },
    #[serde(alias = "Union")]
    Union,
    #[serde(alias = "VectorOnly")]
    VectorOnly,
    #[serde(alias = "KeywordOnly")]
    KeywordOnly,
    #[serde(alias = "Custom")]
    Custom {
        name: String,
        params: serde_json::Value,
    },
}

impl TryFrom<RawFusionStrategy> for FusionStrategy {
    type Error = FusionStrategyError;

    fn try_from(raw: RawFusionStrategy) -> Result<Self, Self::Error> {
        match raw {
            RawFusionStrategy::Rrf { k } => FusionStrategy::try_rrf(k),
            RawFusionStrategy::Weighted { weights } => FusionStrategy::try_weighted(weights),
            RawFusionStrategy::WeightedRrf { k, weights } => {
                FusionStrategy::try_weighted_rrf(k, weights)
            }
            RawFusionStrategy::Union => Ok(FusionStrategy::Union),
            RawFusionStrategy::VectorOnly => Ok(FusionStrategy::VectorOnly),
            RawFusionStrategy::KeywordOnly => Ok(FusionStrategy::KeywordOnly),
            RawFusionStrategy::Custom { name, params } => FusionStrategy::try_custom(name, params),
        }
    }
}

/// Validated selection for combining ranked result lists.
///
/// See `crates/khive-fusion/docs/api/strategy-validation.md`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(try_from = "RawFusionStrategy")]
pub enum FusionStrategy {
    /// Reciprocal Rank Fusion (default, recommended). Rank-based, distribution-agnostic.
    Rrf {
        /// Smoothing constant (>= 1). Default: 60.
        k: usize,
    },

    /// Weighted linear combination of scores. Weights normalized to 1.0; must be finite.
    Weighted {
        /// Weights for each source (will be normalized). Must be finite.
        weights: Vec<f64>,
    },

    /// Weighted Reciprocal Rank Fusion over ordered sources. Weights are not normalized.
    WeightedRrf {
        /// Smoothing constant (>= 1).
        k: usize,
        /// One finite, strictly positive weight for each ordered source.
        weights: Vec<f64>,
    },

    /// Take union with max score per ID.
    Union,

    /// Skip BM25 entirely -- return only vector (HNSW) results.
    VectorOnly,

    /// Skip HNSW entirely -- return only BM25 keyword results.
    KeywordOnly,

    /// Pack-defined or user-defined custom strategy dispatched by name at runtime.
    Custom {
        /// Strategy identifier registered with the fusion executor registry.
        name: String,
        /// Opaque parameters consumed by the executor.
        params: serde_json::Value,
    },
}

impl<'de> Deserialize<'de> for FusionStrategy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawFusionStrategy::deserialize(deserializer)?;
        FusionStrategy::try_from(raw).map_err(serde::de::Error::custom)
    }
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

    /// Create an RRF strategy, returning [`FusionStrategyError::RrfKZero`] for zero.
    #[inline]
    pub fn try_rrf(k: usize) -> Result<Self, FusionStrategyError> {
        if k == 0 {
            return Err(FusionStrategyError::RrfKZero);
        }
        Ok(Self::Rrf { k })
    }

    /// Create an RRF strategy, clamping k to at least 1.
    ///
    /// Prefer [`try_rrf`](Self::try_rrf) at public API boundaries.
    #[inline]
    pub fn rrf_with_k(k: usize) -> Self {
        Self::Rrf { k: k.max(1) }
    }

    /// Create weighted fusion, returning an error for NaN or infinite weights.
    pub fn try_weighted(weights: Vec<f64>) -> Result<Self, FusionStrategyError> {
        for w in &weights {
            if w.is_nan() {
                return Err(FusionStrategyError::WeightNaN);
            }
            if w.is_infinite() {
                return Err(FusionStrategyError::WeightInfinite);
            }
        }
        Ok(Self::Weighted { weights })
    }

    /// Create a weighted strategy. Panics on NaN/infinity.
    ///
    /// Prefer [`try_weighted`](Self::try_weighted) at public API boundaries.
    #[inline]
    pub fn weighted(weights: Vec<f64>) -> Self {
        Self::try_weighted(weights).expect("weights must be finite")
    }

    /// Create weighted reciprocal rank fusion, rejecting zero `k` and weights that are
    /// non-finite or not strictly positive. Source-count validation happens when the strategy is
    /// fused because a strategy does not carry its source lists.
    pub fn try_weighted_rrf(k: usize, weights: Vec<f64>) -> Result<Self, FusionStrategyError> {
        if k == 0 {
            return Err(FusionStrategyError::RrfKZero);
        }
        for weight in &weights {
            if weight.is_nan() {
                return Err(FusionStrategyError::WeightNaN);
            }
            if weight.is_infinite() {
                return Err(FusionStrategyError::WeightInfinite);
            }
            if *weight <= 0.0 {
                return Err(FusionStrategyError::WeightNotPositive);
            }
        }
        Ok(Self::WeightedRrf { k, weights })
    }

    /// Create weighted reciprocal rank fusion. Panics on zero `k` or invalid weights.
    ///
    /// Prefer [`try_weighted_rrf`](Self::try_weighted_rrf) at public API boundaries.
    #[inline]
    pub fn weighted_rrf(k: usize, weights: Vec<f64>) -> Self {
        Self::try_weighted_rrf(k, weights)
            .expect("weighted RRF requires k >= 1 and positive finite weights")
    }

    /// Create a union strategy.
    #[inline]
    pub fn union() -> Self {
        Self::Union
    }

    /// Create a runtime-dispatched custom strategy, rejecting an empty name.
    pub fn try_custom(
        name: String,
        params: serde_json::Value,
    ) -> Result<Self, FusionStrategyError> {
        if name.is_empty() {
            return Err(FusionStrategyError::CustomNameEmpty);
        }
        Ok(Self::Custom { name, params })
    }
}

#[cfg(test)]
#[path = "strategy_tests.rs"]
mod tests;
