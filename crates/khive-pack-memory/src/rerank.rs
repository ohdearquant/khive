//! Weighted feature-combination reranking for memory recall candidates.
//! See `crates/khive-pack-memory/docs/api/scoring.md`.

use std::collections::HashMap;

/// Input features per recall candidate for weighted reranking (relevance, salience, temporal, text_match, vector_match).
#[derive(Debug, Clone)]
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
}

/// Weighted feature-combination rerank score, normalized before accumulation.
/// Returns 0.0 when weights are empty or all recognized weights are zero.
/// RecallConfig::validate owns finite/non-negative weight validation.
/// For finite features in [0, 1], scaling prevents intermediate overflow and
/// avoids multiplying features by subnormal raw weights. Fixed feature order
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
    ];
    // Unknown feature names remain ignored, including in the scale.
    let scale = weighted_features
        .iter()
        .map(|(w, _)| *w)
        .fold(0.0_f64, f64::max);
    if scale == 0.0 {
        return 0.0;
    }
    let mut numerator = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    for (weight, feature_value) in weighted_features {
        if weight == 0.0 {
            continue;
        }
        let normalized_weight = weight / scale;
        numerator += normalized_weight * feature_value;
        if normalized_weight > 0.0 {
            weight_sum += normalized_weight;
        }
    }
    numerator / weight_sum
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn features() -> RerankFeatures {
        RerankFeatures {
            relevance: 0.8,
            salience: 0.6,
            temporal: 0.4,
            text_match: true,
            vector_match: false,
        }
    }

    #[test]
    fn empty_weights_returns_zero() {
        let score = weighted_rerank(&features(), &HashMap::new());
        assert_eq!(score, 0.0, "empty weights must return 0.0");
    }

    #[test]
    fn single_relevance_weight_produces_expected_score() {
        let weights: HashMap<String, f64> = [("relevance".to_string(), 1.0)].into_iter().collect();
        let score = weighted_rerank(&features(), &weights);
        let diff = (score - 0.8).abs();
        assert!(
            diff < 1e-12,
            "relevance weight=1.0 on relevance=0.8 should give 0.8, got {score}"
        );
    }

    #[test]
    fn single_salience_weight_produces_expected_score() {
        // After normalization: (2.0 * 0.6) / 2.0 = 0.6 — the weight magnitude
        // cancels out; only the feature value remains.
        let weights: HashMap<String, f64> = [("salience".to_string(), 2.0)].into_iter().collect();
        let score = weighted_rerank(&features(), &weights);
        let diff = (score - 0.6).abs();
        assert!(
            diff < 1e-12,
            "salience weight=2.0 on salience=0.6 should normalize to 0.6, got {score}"
        );
    }

    #[test]
    fn multi_feature_weight_produces_expected_combination() {
        // relevance*0.5 + salience*0.3 + temporal*0.2
        // = 0.8*0.5 + 0.6*0.3 + 0.4*0.2
        // = 0.40 + 0.18 + 0.08 = 0.66
        let weights: HashMap<String, f64> = [
            ("relevance".to_string(), 0.5),
            ("salience".to_string(), 0.3),
            ("temporal".to_string(), 0.2),
        ]
        .into_iter()
        .collect();
        let score = weighted_rerank(&features(), &weights);
        let diff = (score - 0.66).abs();
        assert!(
            diff < 1e-12,
            "multi-feature combination should give 0.66, got {score}"
        );
    }

    #[test]
    fn boolean_text_match_feature() {
        // text_match=true → 1.0; vector_match=false → 0.0
        // Normalized: (1.0*0.1 + 0.0*0.5) / (0.1 + 0.5) = 0.1 / 0.6 ≈ 0.16667
        let weights: HashMap<String, f64> = [
            ("text_match".to_string(), 0.1),
            ("vector_match".to_string(), 0.5),
        ]
        .into_iter()
        .collect();
        let score = weighted_rerank(&features(), &weights);
        let expected = 0.1_f64 / 0.6_f64;
        let diff = (score - expected).abs();
        assert!(
            diff < 1e-12,
            "boolean features: (text_match*0.1 + vector_match*0.5) / 0.6 ≈ 0.16667, got {score}"
        );
    }

    #[test]
    fn unknown_feature_key_is_silently_ignored() {
        let weights: HashMap<String, f64> = [
            ("relevance".to_string(), 1.0),
            ("future_feature_xyz".to_string(), 999.0),
        ]
        .into_iter()
        .collect();
        let score = weighted_rerank(&features(), &weights);
        // Only relevance should contribute: 0.8*1.0 = 0.8
        let diff = (score - 0.8).abs();
        assert!(
            diff < 1e-12,
            "unknown key should be ignored, expected 0.8, got {score}"
        );
    }

    #[test]
    fn zero_weight_entry_is_skipped() {
        let weights: HashMap<String, f64> = [
            ("relevance".to_string(), 0.0),
            ("salience".to_string(), 1.0),
        ]
        .into_iter()
        .collect();
        let score = weighted_rerank(&features(), &weights);
        // Only salience contributes: (0.6*1.0) / 1.0 = 0.6
        let diff = (score - 0.6).abs();
        assert!(
            diff < 1e-12,
            "zero-weight key should not contribute, expected 0.6, got {score}"
        );
    }

    /// Scaling all positive weights must not change the normalized score.
    #[test]
    fn doubling_all_weights_does_not_change_score() {
        let weights_1x: HashMap<String, f64> = [
            ("relevance".to_string(), 1.0),
            ("salience".to_string(), 0.3),
        ]
        .into_iter()
        .collect();
        let weights_2x: HashMap<String, f64> = [
            ("relevance".to_string(), 2.0),
            ("salience".to_string(), 0.6),
        ]
        .into_iter()
        .collect();
        let score_1x = weighted_rerank(&features(), &weights_1x);
        let score_2x = weighted_rerank(&features(), &weights_2x);
        let diff = (score_1x - score_2x).abs();
        assert!(
            diff < 1e-12,
            "doubling all weights must produce identical score: 1x={score_1x} 2x={score_2x}"
        );
    }

    /// One positive weight returns its feature value because magnitude cancels.
    #[test]
    fn single_weight_of_any_magnitude_returns_feature_value() {
        let f = features(); // relevance=0.8
        for &mag in &[0.5_f64, 1.0, 2.0, 100.0] {
            let weights: HashMap<String, f64> =
                [("relevance".to_string(), mag)].into_iter().collect();
            let score = weighted_rerank(&f, &weights);
            let diff = (score - f.relevance).abs();
            assert!(
                diff < 1e-12,
                "single weight={mag}: expected feature value {}, got {score}",
                f.relevance
            );
        }
    }

    fn extreme_weights(magnitude: f64) -> HashMap<String, f64> {
        [
            ("relevance".to_string(), magnitude),
            ("salience".to_string(), magnitude),
        ]
        .into_iter()
        .collect()
    }

    fn assert_close(got: f64, expected: f64) {
        assert!(
            got.is_finite() && (got - expected).abs() < 1e-12,
            "expected {expected}, got {got}"
        );
    }

    #[test]
    fn ordinary_equal_weights_produce_expected_score() {
        assert_close(weighted_rerank(&features(), &extreme_weights(1.0)), 0.7);
    }

    #[test]
    fn finite_weights_with_overflowing_sum_stay_normalized() {
        assert_close(weighted_rerank(&features(), &extreme_weights(1e308)), 0.7);
    }

    #[test]
    fn finite_weights_with_overflowing_numerator_and_sum_stay_normalized() {
        assert_close(
            weighted_rerank(&features(), &extreme_weights(f64::MAX)),
            0.7,
        );
    }

    #[test]
    fn smallest_subnormal_weights_stay_normalized() {
        assert_close(
            weighted_rerank(&features(), &extreme_weights(f64::from_bits(1))),
            0.7,
        );
    }

    #[test]
    fn positive_weight_magnitudes_across_the_full_exponent_range_stay_normalized() {
        for exponent in [-1074, -1000, -500, 0, 500, 1000, 1023] {
            // Construct the subnormal boundary exactly rather than relying on
            // powi underflow.
            let magnitude = if exponent == -1074 {
                f64::from_bits(1)
            } else {
                2.0_f64.powi(exponent)
            };
            assert_close(
                weighted_rerank(&features(), &extreme_weights(magnitude)),
                0.7,
            );
        }
    }

    #[test]
    fn unknown_weight_does_not_set_the_normalization_scale() {
        let mut weights = extreme_weights(f64::from_bits(1));
        weights.insert("future_feature_xyz".to_string(), f64::MAX);
        assert_close(weighted_rerank(&features(), &weights), 0.7);
    }

    #[test]
    fn zero_and_unknown_weights_only_remain_zero() {
        assert_close(weighted_rerank(&features(), &HashMap::new()), 0.0);
        let weights: HashMap<String, f64> =
            [("future".to_string(), 42.0), ("salience".to_string(), 0.0)]
                .into_iter()
                .collect();
        assert_close(weighted_rerank(&features(), &weights), 0.0);
    }

    #[test]
    fn nan_in_an_unweighted_feature_does_not_contaminate_the_score() {
        let mut f = features();
        f.temporal = f64::NAN;
        assert_close(weighted_rerank(&f, &extreme_weights(1.0)), 0.7);
    }

    #[test]
    fn single_subnormal_weight_still_returns_its_feature_value() {
        let weights: HashMap<String, f64> = [("relevance".to_string(), f64::from_bits(1))]
            .into_iter()
            .collect();
        assert_close(weighted_rerank(&features(), &weights), 0.8);
    }

    /// The score is computed from fixed-order lookups into the weight map,
    /// never by iterating it, so the order the caller inserted keys in must
    /// not change the result's bit pattern.
    #[test]
    fn weight_map_insertion_order_does_not_change_the_score_bits() {
        let mut f = features();
        f.relevance = 1.0;
        f.salience = 1e-16;
        f.temporal = 1e-16;
        let orders = [
            ["relevance", "salience", "temporal"],
            ["relevance", "temporal", "salience"],
            ["salience", "relevance", "temporal"],
            ["salience", "temporal", "relevance"],
            ["temporal", "relevance", "salience"],
            ["temporal", "salience", "relevance"],
        ];
        let mut reference: Option<u64> = None;
        for order in orders {
            let weights: HashMap<String, f64> = order
                .into_iter()
                .map(|key| (key.to_string(), 1.0))
                .collect();
            let bits = weighted_rerank(&f, &weights).to_bits();
            match reference {
                Some(expected) => assert_eq!(
                    bits, expected,
                    "insertion order {order:?} changed the score's bit pattern"
                ),
                None => reference = Some(bits),
            }
        }
    }
}
