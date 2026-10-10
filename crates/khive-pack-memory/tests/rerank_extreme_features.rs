use khive_pack_memory::config::RecallConfig;
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
        graph_proximity: 0.0,
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
        graph_proximity: 0.0,
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

#[test]
fn default_features_and_recall_weights_keep_graph_opt_in() {
    let features = RerankFeatures::default();
    for value in [
        features.relevance,
        features.salience,
        features.temporal,
        features.graph_proximity,
    ] {
        assert_eq!(value.to_bits(), 0.0_f64.to_bits());
    }
    assert!(!features.text_match && !features.vector_match);
    let config = RecallConfig::default();
    assert!(config.reranker_weights.is_empty());
    assert_eq!(weighted_rerank(&features, &config.reranker_weights), 0.0);
}

#[test]
fn precomputed_graph_value_participates_in_normalization() {
    let mut features = RerankFeatures {
        relevance: 0.5,
        graph_proximity: 1.0,
        ..Default::default()
    };
    let weights = [("relevance".into(), 0.6), ("graph_proximity".into(), 0.4)]
        .into_iter()
        .collect();
    let score = weighted_rerank(&features, &weights);
    assert!((score - 0.7).abs() < 1e-12, "got {score}");
    features.graph_proximity = 0.0;
    let score = weighted_rerank(&features, &weights);
    assert!((score - 0.3).abs() < 1e-12, "got {score}");
}

#[test]
fn graph_only_weight_returns_the_supplied_finite_feature() {
    let weights = [("graph_proximity".into(), 7.0)].into_iter().collect();
    for value in [
        -f64::MAX,
        -0.25,
        0.0,
        f64::from_bits(1),
        0.125,
        1.0,
        f64::MAX,
    ] {
        let features = RerankFeatures {
            graph_proximity: value,
            ..Default::default()
        };
        assert_eq!(weighted_rerank(&features, &weights), value);
    }
}

#[test]
fn absent_and_signed_zero_graph_weights_preserve_all_five_feature_score_bits() {
    let features = RerankFeatures {
        relevance: 0.125,
        salience: 0.25,
        temporal: 0.5,
        text_match: true,
        vector_match: false,
        graph_proximity: f64::NAN,
    };
    let mut weights: HashMap<String, f64> = [
        "relevance",
        "salience",
        "temporal",
        "text_match",
        "vector_match",
    ]
    .into_iter()
    .map(|key| (key.into(), 1.0))
    .collect();
    // The five unchanged features have mean (0.125 + 0.25 + 0.5 + 1 + 0) / 5.
    let expected = 0.375_f64.to_bits();
    assert_eq!(weighted_rerank(&features, &weights).to_bits(), expected);
    weights.insert("unknown".into(), f64::MAX);
    for zero in [0.0, -0.0] {
        weights.insert("graph_proximity".into(), zero);
        assert_eq!(weighted_rerank(&features, &weights).to_bits(), expected);
    }
}

#[test]
fn graph_weights_across_finite_scales_keep_the_same_mean() {
    let features = RerankFeatures {
        relevance: 0.5,
        graph_proximity: 1.0,
        ..Default::default()
    };
    for magnitude in [f64::from_bits(1), f64::MIN_POSITIVE, 1.0, 2.0, f64::MAX] {
        let weights = [
            ("relevance".into(), magnitude),
            ("graph_proximity".into(), magnitude),
            ("unknown".into(), f64::MAX),
        ]
        .into_iter()
        .collect();
        assert_eq!(weighted_rerank(&features, &weights), 0.75);
    }
}

#[test]
fn graph_extremes_keep_finite_and_opposing_means() {
    let mut features = RerankFeatures {
        relevance: f64::MAX,
        graph_proximity: f64::MAX,
        ..Default::default()
    };
    let weights = [
        ("relevance".into(), f64::MAX),
        ("graph_proximity".into(), f64::MAX),
    ]
    .into_iter()
    .collect();
    assert_eq!(weighted_rerank(&features, &weights), f64::MAX);
    features.graph_proximity = -f64::MAX;
    assert_eq!(weighted_rerank(&features, &weights), 0.0);
}

#[test]
fn graph_weight_insertion_order_does_not_change_score_bits() {
    let features = RerankFeatures {
        relevance: 0.5,
        salience: 1e-16,
        graph_proximity: 1.0,
        ..Default::default()
    };
    let entries = [
        ("relevance".to_string(), 0.6),
        ("salience".to_string(), 0.1),
        ("graph_proximity".to_string(), 0.4),
        ("unknown".to_string(), f64::MAX),
    ];
    let forward = entries.iter().cloned().collect();
    let reverse = entries.iter().rev().cloned().collect();
    assert_eq!(
        weighted_rerank(&features, &forward).to_bits(),
        weighted_rerank(&features, &reverse).to_bits()
    );
}

#[test]
fn graph_weights_use_the_existing_config_validation_contract() {
    for weight in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let config = RecallConfig {
            reranker_weights: [("graph_proximity".into(), weight)].into_iter().collect(),
            ..Default::default()
        };
        assert!(matches!(
            config.validate(),
            Err(khive_runtime::RuntimeError::InvalidInput(message))
                if message == "reranker_weights[\"graph_proximity\"] must be a finite non-negative number"
        ));
    }
}
