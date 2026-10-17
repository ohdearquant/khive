//! Weighted feature-combination reranking for memory recall candidates.
//! See `crates/khive-pack-memory/docs/api/scoring.md`.

use std::collections::HashMap;

/// Precomputed recall features; numeric values default to zero and membership flags to false.
#[derive(Debug, Clone, Default)]
pub struct RerankFeatures {
    /// Fused retrieval score from RRF or weighted fusion.
    pub relevance: f64,
    /// Decay-adjusted salience value (raw salience × decay factor).
    pub salience: f64,
    /// Half-life–decay recency score independent of per-note decay_factor.
    pub temporal: f64,
    /// True when candidate appeared in FTS text search results.
    pub text_match: bool,
    /// True when candidate appeared in vector search results.
    pub vector_match: bool,
    /// Precomputed graph proximity; the recall handlers currently supply zero.
    pub graph_proximity: f64,
}

/// Weighted feature-combination rerank score, normalized before accumulation.
/// Returns 0.0 when weights are empty or all recognized weights are zero.
/// RecallConfig::validate owns finite/non-negative weight validation.
/// Scaling weights and finite feature magnitudes keeps intermediate sums bounded
/// and avoids multiplying features by subnormal raw weights. Fixed feature order
/// also removes HashMap iteration order from floating-point accumulation.
pub fn weighted_rerank(features: &RerankFeatures, weights: &HashMap<String, f64>) -> f64 {
    let weighted_features = [
        (
            weights.get("relevance").copied().unwrap_or(0.0),
            features.relevance,
        ),
        (
            weights.get("salience").copied().unwrap_or(0.0),
            features.salience,
        ),
        (
            weights.get("temporal").copied().unwrap_or(0.0),
            features.temporal,
        ),
        (
            weights.get("text_match").copied().unwrap_or(0.0),
            f64::from(features.text_match),
        ),
        (
            weights.get("vector_match").copied().unwrap_or(0.0),
            f64::from(features.vector_match),
        ),
        (
            weights.get("graph_proximity").copied().unwrap_or(0.0),
            features.graph_proximity,
        ),
    ];
    // Unknown feature names remain ignored, including in the scale.
    let scale = weighted_features
        .iter()
        .map(|(w, _)| *w)
        .fold(0.0_f64, f64::max);
    if scale == 0.0 {
        return 0.0;
    }
    let feature_scale = weighted_features
        .iter()
        .filter(|(weight, _)| *weight != 0.0)
        .map(|(_, value)| value.abs())
        .fold(1.0_f64, f64::max);
    let mut numerator = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    for (weight, feature_value) in weighted_features {
        if weight == 0.0 {
            continue;
        }
        let normalized_weight = weight / scale;
        numerator += normalized_weight * (feature_value / feature_scale);
        if normalized_weight > 0.0 {
            weight_sum += normalized_weight;
        }
    }
    (numerator / weight_sum).clamp(-1.0, 1.0) * feature_scale
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "rerank_tests.rs"]
mod tests;
