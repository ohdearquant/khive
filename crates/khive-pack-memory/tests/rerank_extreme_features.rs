use khive_pack_memory::rerank::{weighted_rerank, RerankFeatures};
use std::collections::HashMap;

#[test]
fn two_equal_max_weight_finite_max_features_stay_finite() {
    let features = RerankFeatures {
        relevance: f64::MAX,
        salience: 0.0,
        temporal: f64::MAX,
        text_match: false,
        vector_match: false,
    };
    let weights: HashMap<String, f64> = [
        ("relevance".to_string(), 0.5),
        ("temporal".to_string(), 0.5),
    ]
    .into_iter()
    .collect();
    let score = weighted_rerank(&features, &weights);
    assert!(
        score.is_finite(),
        "two finite f64::MAX features at equal max weight must not overflow to infinity, got {score}"
    );
    assert_eq!(
        score,
        f64::MAX,
        "equal weights must return the weighted mean"
    );
}

#[test]
fn opposing_max_features_keep_weighted_mean() {
    let features = RerankFeatures {
        relevance: f64::MAX,
        salience: f64::MAX,
        temporal: -f64::MAX,
        text_match: false,
        vector_match: false,
    };
    let weights: HashMap<String, f64> = [
        ("relevance".to_string(), 0.5),
        ("salience".to_string(), 0.5),
        ("temporal".to_string(), 0.5),
    ]
    .into_iter()
    .collect();
    let score = weighted_rerank(&features, &weights);
    let expected = f64::MAX / 3.0;
    assert!(
        score.is_finite(),
        "finite features must yield a finite score"
    );
    assert!(
        (score / expected - 1.0).abs() < 1e-12,
        "opposing extremes must preserve their weighted mean: expected {expected}, got {score}"
    );
}
