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

    let results = fuse_search_results(vec![vector, keyword], &config);

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
    let results = fuse_search_results::<&str>(vec![], &config);
    assert!(results.is_empty());
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
    let results = fuse_search_results(vec![source], &config);
    assert_eq!(
        results.len(),
        5,
        "single-source result must be truncated to top_k=5"
    );
    assert_eq!(results[0].0, "doc_0", "highest score must be first");
}
