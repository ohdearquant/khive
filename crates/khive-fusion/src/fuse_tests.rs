use super::*;

fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
    items
        .into_iter()
        .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
        .collect()
}

#[test]
fn test_fuse_rrf_strategy() {
    let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
    let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

    assert_eq!(fused.len(), 2);
}

#[test]
fn test_fuse_weighted_strategy() {
    let source = make_results(vec![("doc_a", 1.0)]);
    let fused = fuse(vec![source], &FusionStrategy::weighted(vec![1.0]), 10).unwrap();

    assert_eq!(fused.len(), 1);
}

#[test]
fn test_fuse_union_strategy() {
    let source = make_results(vec![("doc_a", 0.9)]);
    let fused = fuse(vec![source], &FusionStrategy::union(), 10).unwrap();

    assert_eq!(fused.len(), 1);
}

#[test]
fn test_fuse_top_k_truncation() {
    let source = make_results(vec![
        ("doc_a", 0.9),
        ("doc_b", 0.8),
        ("doc_c", 0.7),
        ("doc_d", 0.6),
        ("doc_e", 0.5),
    ]);

    let fused = fuse(vec![source], &FusionStrategy::rrf(), 3).unwrap();

    assert_eq!(fused.len(), 3);
    assert_eq!(fused[0].0, "doc_a");
    assert_eq!(fused[1].0, "doc_b");
    assert_eq!(fused[2].0, "doc_c");
}

#[test]
fn test_fuse_top_k_zero() {
    let source = make_results(vec![("doc_a", 0.9)]);
    let fused = fuse(vec![source], &FusionStrategy::rrf(), 0).unwrap();

    assert!(fused.is_empty());
}

#[test]
fn test_fuse_empty_sources() {
    let fused: Vec<(&str, DeterministicScore)> = fuse(vec![], &FusionStrategy::rrf(), 10).unwrap();
    assert!(fused.is_empty());
}

#[test]
fn test_fuse_top_k_larger_than_results() {
    let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
    let fused = fuse(vec![source], &FusionStrategy::rrf(), 100).unwrap();

    assert_eq!(fused.len(), 2);
}

#[test]
fn test_fuse_with_string_ids() {
    let source: Vec<(String, DeterministicScore)> = vec![
        ("doc_a".to_string(), DeterministicScore::from_f64(0.9)),
        ("doc_b".to_string(), DeterministicScore::from_f64(0.8)),
    ];

    let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].0, "doc_a");
}

#[test]
fn test_fuse_with_integer_ids() {
    let source: Vec<(u64, DeterministicScore)> = vec![
        (1, DeterministicScore::from_f64(0.9)),
        (2, DeterministicScore::from_f64(0.8)),
    ];

    let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].0, 1);
}

#[test]
fn vector_only_two_sources_returns_only_vector_source() {
    let vector = make_results(vec![("vec_only", 0.9)]);
    let keyword = make_results(vec![("kw_only", 1.0)]);
    let out = fuse(vec![vector, keyword], &FusionStrategy::VectorOnly, 10).unwrap();
    let ids: Vec<_> = out.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec!["vec_only"]);
}

#[test]
fn keyword_only_two_sources_returns_only_keyword_source() {
    let vector = make_results(vec![("vec_only", 0.9)]);
    let keyword = make_results(vec![("kw_only", 1.0)]);
    let out = fuse(vec![vector, keyword], &FusionStrategy::KeywordOnly, 10).unwrap();
    let ids: Vec<_> = out.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec!["kw_only"]);
}

#[test]
fn test_fuse_custom_returns_error() {
    let source = make_results(vec![("doc_a", 0.9)]);
    let strategy =
        FusionStrategy::try_custom("decay_weighted".to_string(), serde_json::json!({})).unwrap();
    let result = fuse(vec![source], &strategy, 10);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        FuseError::CustomRequiresRuntime("decay_weighted".to_string())
    );
}

#[test]
fn weighted_rrf_worked_example_and_weight_swap() {
    let first = make_results(vec![("a", 1.0), ("b", 0.5)]);
    let second = make_results(vec![("b", 1.0), ("a", 0.5)]);

    let weighted = FusionStrategy::try_weighted_rrf(10, vec![1.0, 3.0]).unwrap();
    let fused = fuse(vec![first.clone(), second.clone()], &weighted, 10).unwrap();
    assert_eq!(fused[0].0, "b");
    assert!((fused[0].1.to_f64() - (1.0 / 12.0 + 3.0 / 11.0)).abs() < 1e-8);

    let swapped = FusionStrategy::try_weighted_rrf(10, vec![3.0, 1.0]).unwrap();
    let fused = fuse(vec![first, second], &swapped, 10).unwrap();
    assert_eq!(fused[0].0, "a");
}

#[test]
fn weighted_rrf_all_ones_matches_rrf_and_common_scale_doubles_scores() {
    let sources = vec![
        make_results(vec![("a", 0.9), ("b", 0.8), ("a", 0.7)]),
        make_results(vec![("b", 0.9), ("a", 0.8)]),
        Vec::new(),
    ];
    let rrf = fuse(sources.clone(), &FusionStrategy::Rrf { k: 20 }, 10).unwrap();
    let all_ones = FusionStrategy::try_weighted_rrf(20, vec![1.0, 1.0, 1.0]).unwrap();
    let weighted = fuse(sources.clone(), &all_ones, 10).unwrap();
    assert_eq!(weighted, rrf);

    let scaled = FusionStrategy::try_weighted_rrf(20, vec![2.0, 2.0, 2.0]).unwrap();
    let doubled = fuse(sources, &scaled, 10).unwrap();
    for ((id, score), (doubled_id, doubled_score)) in weighted.iter().zip(&doubled) {
        assert_eq!(id, doubled_id);
        assert_eq!(doubled_score.to_raw(), score.to_raw() * 2);
    }
}

#[test]
fn weighted_rrf_keeps_empty_slots_and_duplicate_best_rank() {
    let sources = vec![
        make_results(vec![("a", 1.0), ("b", 0.5), ("a", 0.25)]),
        Vec::new(),
        make_results(vec![("c", 1.0)]),
    ];
    let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0, 2.0, 3.0]).unwrap();
    let fused = fuse(sources, &strategy, 10).unwrap();

    let score_a = fused.iter().find(|(id, _)| *id == "a").unwrap().1.to_f64();
    let score_b = fused.iter().find(|(id, _)| *id == "b").unwrap().1.to_f64();
    let score_c = fused.iter().find(|(id, _)| *id == "c").unwrap().1.to_f64();
    assert!((score_a - 1.0 / 11.0).abs() < 1e-9);
    assert!((score_b - 1.0 / 12.0).abs() < 1e-9);
    assert!((score_c - 3.0 / 11.0).abs() < 1e-9);
    assert_eq!(fused[0].0, "c");
}

#[test]
fn weighted_rrf_ties_use_ascending_id() {
    let sources = vec![
        make_results(vec![("z", 1.0)]),
        make_results(vec![("a", 1.0)]),
    ];
    let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0, 1.0]).unwrap();
    let fused = fuse(sources, &strategy, 10).unwrap();
    assert_eq!(fused[0].0, "a");
    assert_eq!(fused[0].1, fused[1].1);
}

#[test]
fn weighted_rrf_rejects_weight_count_mismatch_even_for_empty_results() {
    let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0]).unwrap();
    let error = fuse(
        vec![Vec::<(&str, DeterministicScore)>::new(), Vec::new()],
        &strategy,
        0,
    )
    .unwrap_err();
    assert_eq!(
        error,
        FuseError::WeightedRrfWeightCountMismatch {
            source_count: 2,
            weight_count: 1
        }
    );
}

#[test]
fn weighted_rrf_rejects_deterministic_score_overflow() {
    let source = make_results(vec![("a", 1.0)]);
    let strategy = FusionStrategy::try_weighted_rrf(1, vec![f64::MAX]).unwrap();
    assert_eq!(
        fuse(vec![source], &strategy, 10),
        Err(FuseError::WeightedRrfScoreOverflow)
    );

    let source_a = make_results(vec![("a", 1.0)]);
    let source_b = make_results(vec![("a", 1.0)]);
    let weight = 4_294_967_294.0;
    let strategy = FusionStrategy::try_weighted_rrf(1, vec![weight, weight]).unwrap();
    assert_eq!(
        fuse(vec![source_a, source_b], &strategy, 10),
        Err(FuseError::WeightedRrfScoreOverflow)
    );
}
