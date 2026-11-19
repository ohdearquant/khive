use super::*;

#[test]
fn test_fuse_empty_sources() {
    let sources: Vec<Vec<(String, DeterministicScore)>> = vec![];
    let config = HybridConfig::default();
    let results = fuse_search_results(sources, &config).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_fuse_single_source() {
    let sources = vec![vec![
        ("a".to_string(), DeterministicScore::from_f64(0.9)),
        ("b".to_string(), DeterministicScore::from_f64(0.8)),
    ]];
    let config = HybridConfig::new(10);
    let results = fuse_search_results(sources, &config).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "a");
    assert_eq!(results[0].1, khive_score::rrf_score(1, 60));
    assert_eq!(results[1].1, khive_score::rrf_score(2, 60));
}

#[test]
fn test_fuse_multiple_sources_rrf() {
    let source1 = vec![
        ("a".to_string(), DeterministicScore::from_f64(0.9)),
        ("b".to_string(), DeterministicScore::from_f64(0.8)),
    ];
    let source2 = vec![
        ("b".to_string(), DeterministicScore::from_f64(0.95)),
        ("c".to_string(), DeterministicScore::from_f64(0.7)),
    ];

    let config = HybridConfig::new(10);
    let results = fuse_search_results(vec![source1, source2], &config).unwrap();

    assert_eq!(results.len(), 3);
    // b appears in both, should have highest RRF score
    assert_eq!(results[0].0, "b");
}

#[test]
fn weighted_sources_use_vector_then_keyword_order() {
    let vector = vec![("vector".to_string(), DeterministicScore::from_f64(0.9))];
    let keyword = vec![("keyword".to_string(), DeterministicScore::from_f64(0.9))];
    let config = HybridConfig::new(10)
        .with_fusion_strategy(FusionStrategy::weighted(vec![0.7, 0.3]))
        .with_weights(0.7, 0.3);

    let results = fuse_search_results(vec![vector, keyword], &config).unwrap();

    assert_eq!(results[0].0, "vector");
    assert!(results[0].1 > results[1].1);
}

#[test]
fn test_fuse_with_min_score() {
    let sources = vec![vec![
        ("a".to_string(), DeterministicScore::from_f64(0.9)),
        ("b".to_string(), DeterministicScore::from_f64(0.1)),
    ]];

    let config = HybridConfig::new(10).with_min_score(DeterministicScore::from_f64(0.5));
    let results = fuse_search_results(sources, &config).unwrap();

    assert!(
        results.is_empty(),
        "the one-arm RRF transform must run before the fused-domain score floor"
    );
}

#[test]
fn test_fuse_top_k_limit() {
    let sources = vec![vec![
        ("a".to_string(), DeterministicScore::from_f64(0.9)),
        ("b".to_string(), DeterministicScore::from_f64(0.8)),
        ("c".to_string(), DeterministicScore::from_f64(0.7)),
        ("d".to_string(), DeterministicScore::from_f64(0.6)),
        ("e".to_string(), DeterministicScore::from_f64(0.5)),
    ]];

    let config = HybridConfig::new(3);
    let results = fuse_search_results(sources, &config).unwrap();

    assert_eq!(results.len(), 3);
}

#[test]
fn test_fuse_weighted_three_sources_falls_back_to_rrf() {
    // Regression guard: Weighted with 3 sources previously used debug_assert
    // which was a no-op in release builds. Now it must fall back to RRF (not panic).
    use khive_fusion::FusionStrategy;
    let source1 = vec![("a".to_string(), DeterministicScore::from_f64(0.9))];
    let source2 = vec![("b".to_string(), DeterministicScore::from_f64(0.8))];
    let source3 = vec![("c".to_string(), DeterministicScore::from_f64(0.7))];

    let config =
        HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted(vec![0.5, 0.5]));

    // Must not panic — falls back to RRF silently.
    let results = fuse_search_results(vec![source1, source2, source3], &config).unwrap();
    assert_eq!(
        results.len(),
        3,
        "all 3 results should survive RRF fallback"
    );
}

#[test]
fn test_fuse_search_results_checked_weighted_wrong_count_returns_err() {
    use khive_fusion::FusionStrategy;
    let config =
        HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted(vec![0.5, 0.5]));

    let source1 = vec![("a".to_string(), DeterministicScore::from_f64(0.9))];
    let source2 = vec![("b".to_string(), DeterministicScore::from_f64(0.8))];
    let source3 = vec![("c".to_string(), DeterministicScore::from_f64(0.7))];

    let result = fuse_search_results_checked(vec![source1, source2, source3], &config);
    assert!(
        result.is_err(),
        "checked variant must return Err for 3-source Weighted fusion"
    );
}

#[test]
fn test_fuse_search_results_checked_weighted_two_sources_ok() {
    use khive_fusion::FusionStrategy;
    let config =
        HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted(vec![0.5, 0.5]));

    let source1 = vec![("a".to_string(), DeterministicScore::from_f64(0.9))];
    let source2 = vec![("b".to_string(), DeterministicScore::from_f64(0.8))];

    let result = fuse_search_results_checked(vec![source1, source2], &config);
    assert!(result.is_ok(), "2-source Weighted must succeed");
    assert_eq!(result.unwrap().len(), 2);
}

#[test]
fn test_fuse_search_results_checked_weighted_empty_arm_keeps_slot() {
    let config = HybridConfig::new(10)
        .with_fusion_strategy(FusionStrategy::weighted(vec![0.7, 0.3]))
        .with_weights(0.7, 0.3);
    let keyword = vec![("keyword".to_string(), DeterministicScore::from_f64(0.8))];

    let result = fuse_search_results_checked(vec![Vec::new(), keyword], &config)
        .expect("an empty vector arm still occupies its canonical slot");

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "keyword");
    assert!((result[0].1.to_f64() - 0.3).abs() < 1e-9);
}

#[test]
fn test_fuse_search_results_checked_weighted_rrf_rejects_wrong_count_for_empty_arms() {
    let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0]).unwrap();
    let config = HybridConfig::new(10).with_fusion_strategy(strategy);

    let result = fuse_search_results_checked(
        vec![Vec::<(String, DeterministicScore)>::new(), Vec::new()],
        &config,
    );
    assert!(
        result.is_err(),
        "checked fusion must reject a weight/source mismatch"
    );
}

#[test]
fn test_fuse_search_results_checked_weighted_rrf_reports_score_overflow() {
    let strategy = FusionStrategy::try_weighted_rrf(1, vec![f64::MAX]).unwrap();
    let config = HybridConfig::new(10).with_fusion_strategy(strategy);
    let source = vec![("a".to_string(), DeterministicScore::from_f64(1.0))];

    let result = fuse_search_results_checked(vec![source], &config);
    assert!(
        result.is_err(),
        "checked fusion must surface deterministic-score overflow"
    );
}

#[test]
fn test_fuse_search_results_checked_weighted_rrf_applies_weights_by_slot() {
    let first = vec![
        ("a".to_string(), DeterministicScore::from_f64(1.0)),
        ("b".to_string(), DeterministicScore::from_f64(0.5)),
    ];
    let second = vec![
        ("b".to_string(), DeterministicScore::from_f64(1.0)),
        ("a".to_string(), DeterministicScore::from_f64(0.5)),
    ];
    let sources = vec![first, second];

    let weighted = FusionStrategy::try_weighted_rrf(10, vec![1.0, 3.0]).unwrap();
    let config = HybridConfig::new(10).with_fusion_strategy(weighted);
    let result = fuse_search_results_checked(sources.clone(), &config).unwrap();
    assert_eq!(result[0].0, "b");

    let swapped = FusionStrategy::try_weighted_rrf(10, vec![3.0, 1.0]).unwrap();
    let config = HybridConfig::new(10).with_fusion_strategy(swapped);
    let result = fuse_search_results_checked(sources, &config).unwrap();
    assert_eq!(result[0].0, "a");
}

#[test]
fn test_fuse_search_results_weighted_rrf_never_falls_back_to_rrf() {
    let source = vec![("a".to_string(), DeterministicScore::from_f64(1.0))];

    let overflow = FusionStrategy::try_weighted_rrf(1, vec![f64::MAX]).unwrap();
    let config = HybridConfig::new(10).with_fusion_strategy(overflow);
    assert_eq!(
        fuse_search_results(vec![source.clone()], &config),
        Err(FuseError::WeightedRrfScoreOverflow)
    );

    let mismatch = FusionStrategy::try_weighted_rrf(1, vec![1.0, 1.0]).unwrap();
    let config = HybridConfig::new(10).with_fusion_strategy(mismatch);
    assert_eq!(
        fuse_search_results(vec![source], &config),
        Err(FuseError::WeightedRrfWeightCountMismatch {
            source_count: 1,
            weight_count: 2,
        })
    );
}
