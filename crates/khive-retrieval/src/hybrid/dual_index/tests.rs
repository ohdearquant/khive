use super::*;

/// Helper to build scored result lists from (id, f64) pairs.
fn make_results(items: Vec<(&str, f64)>) -> Vec<(String, DeterministicScore)> {
    items
        .into_iter()
        .map(|(id, score)| (id.to_string(), DeterministicScore::from_f64(score)))
        .collect()
}

// -- Strategy routing tests --

#[test]
fn test_primary_only_queries_only_primary() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::PrimaryOnly);
    let router = DualIndexRouter::<String>::new(config);

    assert!(router.should_query_primary(None));
    assert!(!router.should_query_legacy(None));
}

#[test]
fn test_legacy_only_queries_only_legacy() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::LegacyOnly);
    let router = DualIndexRouter::<String>::new(config);

    assert!(!router.should_query_primary(None));
    assert!(router.should_query_legacy(None));
}

#[test]
fn test_both_queries_both_indexes() {
    let config = DualIndexConfig::default(); // default is Both { rrf }
    let router = DualIndexRouter::<String>::new(config);

    assert!(router.should_query_primary(None));
    assert!(router.should_query_legacy(None));
}

#[test]
fn test_weighted_queries_both_indexes() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Weighted {
        primary_weight: 0.8,
    });
    let router = DualIndexRouter::<String>::new(config);

    assert!(router.should_query_primary(None));
    assert!(router.should_query_legacy(None));
}

// -- Auto-switch threshold tests --

#[test]
fn test_auto_switch_skips_legacy_when_threshold_exceeded() {
    let config = DualIndexConfig::default().with_auto_switch_threshold(0.95);
    let router = DualIndexRouter::<String>::new(config);

    // Migration at 90% - below threshold, still query legacy
    assert!(router.should_query_legacy(Some(0.90)));

    // Migration at 95% - at threshold, skip legacy (progress >= threshold)
    assert!(!router.should_query_legacy(Some(0.95)));

    // Migration at 99% - above threshold, skip legacy
    assert!(!router.should_query_legacy(Some(0.99)));
}

#[test]
fn test_auto_switch_no_threshold_always_queries_legacy() {
    let config = DualIndexConfig::default(); // no auto_switch_threshold
    let router = DualIndexRouter::<String>::new(config);

    // Even with 100% progress, queries legacy without threshold
    assert!(router.should_query_legacy(Some(1.0)));
}

#[test]
fn test_auto_switch_no_progress_queries_legacy() {
    let config = DualIndexConfig::default().with_auto_switch_threshold(0.95);
    let router = DualIndexRouter::<String>::new(config);

    // No progress info provided - query legacy to be safe
    assert!(router.should_query_legacy(None));
}

// -- Pool size tests --

#[test]
fn test_pool_size_calculation() {
    let config = DualIndexConfig::default(); // pool_multiplier = 3
    let router = DualIndexRouter::<String>::new(config);

    assert_eq!(router.pool_size(10), 30);
    assert_eq!(router.pool_size(1), 3);
    assert_eq!(router.pool_size(0), 0);
}

#[test]
fn test_pool_size_custom_multiplier() {
    let config = DualIndexConfig::default().with_pool_multiplier(5);
    let router = DualIndexRouter::<String>::new(config);

    assert_eq!(router.pool_size(10), 50);
}

// -- Merge results tests --

#[test]
fn test_merge_primary_only_returns_primary() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::PrimaryOnly);
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
    let legacy = make_results(vec![("c", 0.95), ("d", 0.85)]);

    let merged = router.merge_results(primary, legacy, 10).unwrap();
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].0, "a");
    assert_eq!(merged[1].0, "b");
}

#[test]
fn test_merge_legacy_only_returns_legacy() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::LegacyOnly);
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
    let legacy = make_results(vec![("c", 0.95), ("d", 0.85)]);

    let merged = router.merge_results(primary, legacy, 10).unwrap();
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].0, "c");
    assert_eq!(merged[1].0, "d");
}

#[test]
fn test_merge_both_fuses_with_rrf() {
    let config = DualIndexConfig::default(); // Both { Rrf { k: 60 } }
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
    let legacy = make_results(vec![("b", 0.95), ("c", 0.7)]);

    let merged = router.merge_results(primary, legacy, 10).unwrap();

    // "b" appears in both sources, should get highest RRF score
    assert_eq!(merged[0].0, "b");
    // All three unique IDs should be present
    assert_eq!(merged.len(), 3);
}

#[test]
fn test_merge_both_weighted_rrf_uses_primary_then_legacy_order() {
    let fusion = FusionStrategy::try_weighted_rrf(10, vec![1.0, 3.0]).unwrap();
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Both { fusion });
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
    let legacy = make_results(vec![("b", 0.9), ("a", 0.8)]);

    let merged = router.merge_results(primary, legacy, 10).unwrap();
    assert_eq!(merged[0].0, "b");
}

#[test]
fn test_merge_both_weighted_rrf_with_wrong_weight_count_returns_error() {
    let fusion = FusionStrategy::try_weighted_rrf(10, vec![1.0]).unwrap();
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Both { fusion });
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9)]);
    let legacy = make_results(vec![("b", 0.9)]);

    assert_eq!(
        router.merge_results(primary, legacy, 10),
        Err(FuseError::WeightedRrfWeightCountMismatch {
            source_count: 2,
            weight_count: 1,
        })
    );
    assert_eq!(
        router.merge_results(vec![], vec![], 10),
        Err(FuseError::WeightedRrfWeightCountMismatch {
            source_count: 2,
            weight_count: 1,
        })
    );
}

#[test]
fn test_merge_both_weighted_rrf_preserves_validation_and_score_errors() {
    for (weights, expected) in [
        (
            vec![f64::NAN, 1.0],
            FuseError::InvalidWeightedRrfStrategy(khive_fusion::FusionStrategyError::WeightNaN),
        ),
        (vec![f64::MAX, 1.0], FuseError::WeightedRrfScoreOverflow),
    ] {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Both {
            fusion: FusionStrategy::WeightedRrf { k: 1, weights },
        });
        let router = DualIndexRouter::<String>::new(config);
        assert_eq!(
            router.merge_results(make_results(vec![("a", 1.0)]), vec![], 10),
            Err(expected)
        );
    }
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Both {
        fusion: FusionStrategy::weighted_rrf(1, vec![1.0, 1.0]),
    });
    assert!(DualIndexRouter::<String>::new(config)
        .merge_results(vec![], vec![], 10)
        .unwrap()
        .is_empty());
}

#[test]
fn test_merge_weighted_applies_weights() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Weighted {
        primary_weight: 0.8,
    });
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9), ("b", 0.5)]);
    let legacy = make_results(vec![("b", 0.9), ("c", 0.5)]);

    let merged = router.merge_results(primary, legacy, 10).unwrap();

    // All three unique IDs should appear
    let ids: Vec<&str> = merged.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"b"));
    assert!(ids.contains(&"c"));
}

#[test]
fn test_merge_weighted_uses_primary_then_legacy_order() {
    let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Weighted {
        primary_weight: 0.8,
    });
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("primary", 0.9)]);
    let legacy = make_results(vec![("legacy", 0.9)]);
    let merged = router.merge_results(primary, legacy, 10).unwrap();

    assert_eq!(merged[0].0, "primary");
    assert!(merged[0].1 > merged[1].1);
}

#[test]
fn test_merge_weighted_non_finite_weights_do_not_panic() {
    let cases = [
        (f64::NAN, vec!["a_primary", "z_legacy"]),
        (f64::INFINITY, vec!["a_primary"]),
        (f64::NEG_INFINITY, vec!["z_legacy"]),
    ];

    for (primary_weight, expected_ids) in cases {
        let config = DualIndexConfig::default()
            .with_strategy(DualIndexStrategy::Weighted { primary_weight });
        let router = DualIndexRouter::<String>::new(config);

        let result = std::panic::catch_unwind(|| {
            router.merge_results(
                make_results(vec![("a_primary", 0.9)]),
                make_results(vec![("z_legacy", 0.8)]),
                10,
            )
        });

        assert!(
            result.is_ok(),
            "merge_results must not panic for primary_weight={primary_weight}"
        );

        let merged = result.unwrap().unwrap();
        let ids: Vec<_> = merged.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, expected_ids);
    }
}

#[test]
fn test_merge_respects_top_k() {
    let config = DualIndexConfig::default();
    let router = DualIndexRouter::<String>::new(config);

    let primary = make_results(vec![("a", 0.9), ("b", 0.8), ("c", 0.7)]);
    let legacy = make_results(vec![("d", 0.95), ("e", 0.85), ("f", 0.75)]);

    let merged = router.merge_results(primary, legacy, 2).unwrap();
    assert_eq!(merged.len(), 2);
}

#[test]
fn test_merge_empty_sources() {
    let config = DualIndexConfig::default();
    let router = DualIndexRouter::<String>::new(config);

    let merged = router.merge_results(vec![], vec![], 10).unwrap();
    assert!(merged.is_empty());
}

// -- Strategy mutation tests --

#[test]
fn test_set_strategy_updates_routing() {
    let config = DualIndexConfig::default();
    let mut router = DualIndexRouter::<String>::new(config);

    assert!(matches!(router.strategy(), DualIndexStrategy::Both { .. }));

    router.set_strategy(DualIndexStrategy::PrimaryOnly);
    assert!(matches!(router.strategy(), DualIndexStrategy::PrimaryOnly));
}

// -- Config builder tests --

#[test]
fn test_config_default() {
    let config = DualIndexConfig::default();
    assert!(matches!(config.strategy, DualIndexStrategy::Both { .. }));
    assert_eq!(config.pool_multiplier, 3);
    assert!(config.auto_switch_threshold.is_none());
}

#[test]
fn test_config_builder_chain() {
    let config = DualIndexConfig::default()
        .with_strategy(DualIndexStrategy::Weighted {
            primary_weight: 0.7,
        })
        .with_pool_multiplier(5)
        .with_auto_switch_threshold(0.95);

    assert!(matches!(
        config.strategy,
        DualIndexStrategy::Weighted { primary_weight } if (primary_weight - 0.7).abs() < f64::EPSILON
    ));
    assert_eq!(config.pool_multiplier, 5);
    assert!((config.auto_switch_threshold.unwrap() - 0.95).abs() < f64::EPSILON);
}

#[test]
fn test_pool_multiplier_min_enforced() {
    let config = DualIndexConfig::default().with_pool_multiplier(0);
    assert_eq!(config.pool_multiplier, 1);
}

#[test]
fn test_auto_switch_threshold_clamped() {
    let config = DualIndexConfig::default().with_auto_switch_threshold(1.5);
    assert!((config.auto_switch_threshold.unwrap() - 1.0).abs() < f64::EPSILON);

    let config = DualIndexConfig::default().with_auto_switch_threshold(-0.5);
    assert!((config.auto_switch_threshold.unwrap() - 0.0).abs() < f64::EPSILON);
}

// -- Default strategy tests --

#[test]
fn test_default_strategy_is_both_rrf() {
    let strategy = DualIndexStrategy::default();
    assert_eq!(
        strategy,
        DualIndexStrategy::Both {
            fusion: FusionStrategy::rrf()
        }
    );
}
