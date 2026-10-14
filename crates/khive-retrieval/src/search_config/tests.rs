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
