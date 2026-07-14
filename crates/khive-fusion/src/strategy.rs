//! Fusion strategy types with invariant validation.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Default RRF constant k=60, standard in literature (Craswell et al., 2009).
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
    /// Custom strategy name must not be empty.
    CustomNameEmpty,
}

impl fmt::Display for FusionStrategyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RrfKZero => write!(f, "Rrf k must be >= 1"),
            Self::WeightNaN => write!(f, "Weighted weights must not contain NaN"),
            Self::WeightInfinite => write!(f, "Weighted weights must not contain infinity"),
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
        assert_eq!(FusionStrategy::rrf_with_k(0), FusionStrategy::Rrf { k: 1 });
        assert_eq!(
            FusionStrategy::weighted(vec![0.5, 0.5]),
            FusionStrategy::Weighted {
                weights: vec![0.5, 0.5]
            }
        );
        assert_eq!(FusionStrategy::union(), FusionStrategy::Union);
    }

    #[test]
    fn test_try_rrf_rejects_zero() {
        assert_eq!(
            FusionStrategy::try_rrf(0),
            Err(FusionStrategyError::RrfKZero)
        );
        assert!(FusionStrategy::try_rrf(1).is_ok());
        assert!(FusionStrategy::try_rrf(60).is_ok());
    }

    #[test]
    fn test_try_weighted_rejects_nan() {
        assert_eq!(
            FusionStrategy::try_weighted(vec![0.5, f64::NAN]),
            Err(FusionStrategyError::WeightNaN)
        );
    }

    #[test]
    fn test_try_weighted_rejects_infinity() {
        assert_eq!(
            FusionStrategy::try_weighted(vec![f64::INFINITY, 0.5]),
            Err(FusionStrategyError::WeightInfinite)
        );
        assert_eq!(
            FusionStrategy::try_weighted(vec![0.5, f64::NEG_INFINITY]),
            Err(FusionStrategyError::WeightInfinite)
        );
    }

    #[test]
    fn test_try_weighted_accepts_valid() {
        assert!(FusionStrategy::try_weighted(vec![0.5, 0.5]).is_ok());
        assert!(FusionStrategy::try_weighted(vec![0.0, 0.0]).is_ok());
        assert!(FusionStrategy::try_weighted(vec![-1.0, 1.0]).is_ok());
        assert!(FusionStrategy::try_weighted(vec![]).is_ok());
    }

    #[test]
    fn test_try_custom_rejects_empty_name() {
        assert_eq!(
            FusionStrategy::try_custom(String::new(), serde_json::Value::Null),
            Err(FusionStrategyError::CustomNameEmpty)
        );
    }

    #[test]
    fn test_try_custom_accepts_valid() {
        let result = FusionStrategy::try_custom(
            "decay_weighted".to_string(),
            serde_json::json!({"decay": 0.95}),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_serde_roundtrip_rrf() {
        let strategy = FusionStrategy::Rrf { k: 60 };
        let json = serde_json::to_string(&strategy).unwrap();
        let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(strategy, deserialized);
    }

    #[test]
    fn test_serde_roundtrip_weighted() {
        let strategy = FusionStrategy::Weighted {
            weights: vec![0.6, 0.4],
        };
        let json = serde_json::to_string(&strategy).unwrap();
        let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(strategy, deserialized);
    }

    #[test]
    fn test_serde_roundtrip_custom() {
        let strategy = FusionStrategy::Custom {
            name: "decay_weighted".to_string(),
            params: serde_json::json!({"decay": 0.95}),
        };
        let json = serde_json::to_string(&strategy).unwrap();
        let deserialized: FusionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(strategy, deserialized);
    }

    #[test]
    fn test_serde_rejects_rrf_k_zero() {
        let json = r#"{"rrf":{"k":0}}"#;
        let result: Result<FusionStrategy, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_serde_rejects_nan_weights() {
        // NaN cannot be represented in JSON, so this tests via the builder.
        // JSON with null weight would fail at a different level.
        assert!(FusionStrategy::try_weighted(vec![f64::NAN]).is_err());
    }

    #[test]
    fn test_serde_rejects_custom_empty_name() {
        let json = r#"{"custom":{"name":"","params":null}}"#;
        let result: Result<FusionStrategy, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
