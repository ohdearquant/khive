//! Weighted feature-combination reranking for memory recall candidates.

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

/// Weighted feature-combination rerank score: `Σ(weight × feature) / Σ(positive_weight)`.
/// Returns 0.0 when weights are empty or all unrecognized.
pub fn weighted_rerank(features: &RerankFeatures, weights: &HashMap<String, f64>) -> f64 {
    let mut numerator = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    for (name, &weight) in weights {
        if weight == 0.0 {
            continue;
        }
        let feature_value = match name.as_str() {
            "relevance" => features.relevance,
            "salience" => features.salience,
            "temporal" => features.temporal,
            "text_match" => f64::from(features.text_match),
            "vector_match" => f64::from(features.vector_match),
            // Unknown feature names are silently ignored to allow forward-compat.
            _ => continue,
        };
        numerator += weight * feature_value;
        if weight > 0.0 {
            weight_sum += weight;
        }
    }
    if weight_sum == 0.0 {
        return 0.0;
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

    /// Normalization: doubling all weights must NOT change the output score.
    /// Un-normalized: (0.8*2.0 + 0.6*0.6) / 1 = 1.96 — clearly wrong.
    /// Normalized:    (0.8*2.0 + 0.6*0.6) / (2.0 + 0.6) = same as weight 1.0 + 0.3.
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

    /// Normalization: a single weight of any positive magnitude returns the feature
    /// value directly (the weight cancels out in numerator / denominator).
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
}
