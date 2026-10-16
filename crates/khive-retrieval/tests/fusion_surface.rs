use khive_retrieval::{fuse_search_results, FusionStrategy, HybridConfig};
use khive_score::DeterministicScore;

#[test]
fn fuse_search_results_rrf_surface_matches_expected_order() {
    // doc_b appears at rank 1 in both vector and keyword — must win under RRF k=60.
    let vector = vec![
        ("doc_b", DeterministicScore::from_f64(0.9)),
        ("doc_a", DeterministicScore::from_f64(0.8)),
    ];
    let keyword = vec![
        ("doc_b", DeterministicScore::from_f64(4.0)),
        ("doc_c", DeterministicScore::from_f64(3.0)),
    ];
    let config = HybridConfig::new(10)
        .with_pool_size(10)
        .with_fusion_strategy(FusionStrategy::Rrf { k: 60 });

    let results = fuse_search_results(vec![vector, keyword], &config).unwrap();

    assert!(!results.is_empty(), "fusion must return results");
    assert_eq!(
        results[0].0, "doc_b",
        "doc_b must rank first (appears in both sources)"
    );

    // RRF score for doc_b: 1/(1+60) + 1/(1+60) = 2/61 ≈ 0.03279
    let expected = 2.0 / 61.0;
    let actual = results[0].1.to_f64();
    assert!(
        (actual - expected).abs() < 1e-6,
        "fused score = {actual}, expected ~{expected}"
    );
}

#[test]
fn fuse_search_results_empty_sources_returns_empty() {
    let config = HybridConfig::default();
    let results = fuse_search_results::<&str>(vec![], &config).unwrap();
    assert!(results.is_empty());
}

#[test]
fn fuse_search_results_vector_only_returns_only_vector_source() {
    let vector = vec![("vec_only", DeterministicScore::from_f64(0.9))];
    let keyword = vec![("kw_only", DeterministicScore::from_f64(1.0))];
    let config = HybridConfig::new(10).with_fusion_strategy(FusionStrategy::VectorOnly);
    let results = fuse_search_results(vec![vector, keyword], &config).unwrap();
    let ids: Vec<_> = results.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec!["vec_only"]);
}

#[test]
fn fuse_search_results_keyword_only_returns_only_keyword_source() {
    let vector = vec![("vec_only", DeterministicScore::from_f64(0.9))];
    let keyword = vec![("kw_only", DeterministicScore::from_f64(1.0))];
    let config = HybridConfig::new(10).with_fusion_strategy(FusionStrategy::KeywordOnly);
    let results = fuse_search_results(vec![vector, keyword], &config).unwrap();
    let ids: Vec<_> = results.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec!["kw_only"]);
}

#[test]
fn fuse_search_results_single_source_truncates_to_top_k() {
    let source: Vec<_> = (0..20)
        .map(|i| {
            (
                format!("doc_{i}"),
                DeterministicScore::from_f64(1.0 - i as f64 * 0.01),
            )
        })
        .collect();
    let config = HybridConfig::new(5);
    let results = fuse_search_results(vec![source], &config).unwrap();
    assert_eq!(
        results.len(),
        5,
        "single-source result must be truncated to top_k=5"
    );
    assert_eq!(results[0].0, "doc_0", "highest score must be first");
}

#[test]
fn weighted_rrf_mismatch_is_an_error_even_without_hits_or_requested_results() {
    use khive_fusion::FuseError;

    for source_count in [0, 2] {
        for top_k in [0, 10] {
            let sources = vec![Vec::<(&str, DeterministicScore)>::new(); source_count];
            let config = HybridConfig::new(top_k)
                .with_fusion_strategy(FusionStrategy::weighted_rrf(10, vec![1.0]));
            assert_eq!(
                fuse_search_results(sources, &config),
                Err(FuseError::WeightedRrfWeightCountMismatch {
                    source_count,
                    weight_count: 1,
                })
            );
        }
    }
}

#[test]
fn weighted_rrf_direct_invalid_parameters_preserve_the_cause_on_empty_arms() {
    use khive_fusion::{FuseError, FusionStrategyError};

    for (k, weight, cause) in [
        (0, 1.0, FusionStrategyError::RrfKZero),
        (1, f64::NAN, FusionStrategyError::WeightNaN),
        (1, f64::INFINITY, FusionStrategyError::WeightInfinite),
        (1, f64::NEG_INFINITY, FusionStrategyError::WeightInfinite),
        (1, 0.0, FusionStrategyError::WeightNotPositive),
        (1, -1.0, FusionStrategyError::WeightNotPositive),
    ] {
        let config = HybridConfig::new(10).with_fusion_strategy(FusionStrategy::WeightedRrf {
            k,
            weights: vec![weight],
        });
        assert_eq!(
            fuse_search_results::<&str>(vec![vec![]], &config),
            Err(FuseError::InvalidWeightedRrfStrategy(cause))
        );
    }
}

#[test]
fn weighted_rrf_valid_empty_and_scored_results_remain_successful() {
    for source_count in [0, 2] {
        let config = HybridConfig::new(10)
            .with_fusion_strategy(FusionStrategy::weighted_rrf(10, vec![1.0; source_count]));
        assert!(
            fuse_search_results::<&str>(vec![vec![]; source_count], &config)
                .unwrap()
                .is_empty()
        );
    }

    let sources = vec![
        vec![("a", 1.0.into()), ("b", 0.5.into())],
        vec![("b", 1.0.into()), ("a", 0.5.into())],
    ];
    let config = HybridConfig::new(10)
        .with_fusion_strategy(FusionStrategy::weighted_rrf(10, vec![1.0, 3.0]));
    let results = fuse_search_results(sources, &config).unwrap();
    assert_eq!(
        results.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec!["b", "a"]
    );
    let expected = 1.0 / 12.0 + 3.0 / 11.0;
    assert!((results[0].1.to_f64() - expected).abs() < 1e-9);
}

#[test]
fn weighted_rrf_score_overflow_is_not_an_empty_search() {
    use khive_fusion::FuseError;

    let config =
        HybridConfig::new(10).with_fusion_strategy(FusionStrategy::weighted_rrf(1, vec![f64::MAX]));
    assert_eq!(
        fuse_search_results(vec![vec![("a", 1.0.into())]], &config),
        Err(FuseError::WeightedRrfScoreOverflow)
    );
}
