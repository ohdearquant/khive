use super::*;

fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
    items
        .into_iter()
        .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
        .collect()
}

#[test]
fn test_weighted_basic() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_a", 1.0)]);

    let fused = weighted_fusion(vec![source1, source2], &[0.7, 0.3]);

    assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
}

#[test]
fn test_weighted_normalization() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_a", 1.0)]);

    let fused = weighted_fusion(vec![source1, source2], &[7.0, 3.0]);

    assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
}

#[test]
fn test_weighted_zero_weights() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_a", 1.0)]);

    let fused = weighted_fusion(vec![source1, source2], &[0.0, 0.0]);

    assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
}

#[test]
fn test_weighted_fusion_overflow_sum_falls_back_to_equal_weights() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_b", 1.0)]);

    let fused = weighted_fusion(vec![source1, source2], &[f64::MAX, f64::MAX]);

    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].0, "doc_a");
    assert_eq!(fused[1].0, "doc_b");
    assert!((fused[0].1.to_f64() - 0.5).abs() < 1e-10);
    assert_eq!(fused[0].1, fused[1].1);
}

#[test]
fn test_weighted_fusion_overflow_fallback_excludes_unweighted_source() {
    let sources = vec![
        make_results(vec![("doc_a", 1.0)]),
        make_results(vec![("doc_b", 1.0)]),
        make_results(vec![("doc_c", 1.0)]),
    ];

    let fused = weighted_fusion(sources, &[f64::MAX, f64::MAX]);

    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].0, "doc_a");
    assert_eq!(fused[1].0, "doc_b");
    assert!((fused[0].1.to_f64() - 0.5).abs() < 1e-10);
    assert_eq!(fused[0].1, fused[1].1);
}

#[test]
fn test_weighted_fusion_zero_fallback_excludes_unweighted_source() {
    let sources = vec![
        make_results(vec![("doc_a", 1.0)]),
        make_results(vec![("doc_b", 1.0)]),
        make_results(vec![("doc_c", 1.0)]),
    ];

    let fused = weighted_fusion(sources, &[0.0, 0.0]);

    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].0, "doc_a");
    assert_eq!(fused[1].0, "doc_b");
    assert!((fused[0].1.to_f64() - 0.5).abs() < 1e-10);
    assert_eq!(fused[0].1, fused[1].1);
}

#[test]
fn test_weighted_fusion_empty_weights_fallback_includes_all_sources() {
    let sources = vec![
        make_results(vec![("doc_a", 1.0)]),
        make_results(vec![("doc_b", 1.0)]),
        make_results(vec![("doc_c", 1.0)]),
    ];

    let fused = weighted_fusion(sources, &[]);

    assert_eq!(fused.len(), 3);
    assert_eq!(fused[0].0, "doc_a");
    assert_eq!(fused[1].0, "doc_b");
    assert_eq!(fused[2].0, "doc_c");
    assert!((fused[0].1.to_f64() - 1.0 / 3.0).abs() < 1e-9);
    assert_eq!(fused[0].1, fused[1].1);
    assert_eq!(fused[1].1, fused[2].1);
}

#[test]
fn test_weighted_disjoint_results() {
    let source1 = make_results(vec![("doc_a", 0.9)]);
    let source2 = make_results(vec![("doc_b", 0.8)]);

    let fused = weighted_fusion(vec![source1, source2], &[0.6, 0.4]);

    let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
    let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

    // After per-source min-max normalization, single-element sources map to 1.0.
    // doc_a contributes 1.0 * 0.6 = 0.6, doc_b contributes 1.0 * 0.4 = 0.4.
    assert!((doc_a.1.to_f64() - 0.6).abs() < 0.01);
    assert!((doc_b.1.to_f64() - 0.4).abs() < 0.01);
}

#[test]
fn test_weighted_empty_sources() {
    let fused: Vec<(&str, DeterministicScore)> = weighted_fusion(vec![], &[]);
    assert!(fused.is_empty());
}

#[test]
fn test_weighted_single_source() {
    let source = make_results(vec![("doc_a", 0.9)]);
    let fused = weighted_fusion(vec![source], &[1.0]);

    assert_eq!(fused.len(), 1);
    // Single-element source normalizes to 1.0; weight is 1.0 → final = 1.0.
    assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
}

// RETRIEVAL-07: Normalization behavior tests

#[test]
fn test_normalization_already_normalized() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_b", 1.0)]);

    // Weights already sum to 1.0
    let fused = weighted_fusion(vec![source1, source2], &[0.6, 0.4]);

    let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
    let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

    assert!((doc_a.1.to_f64() - 0.6).abs() < 0.01);
    assert!((doc_b.1.to_f64() - 0.4).abs() < 0.01);
}

#[test]
fn test_normalization_scaled_weights() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_b", 1.0)]);

    // Weights sum to 100, should be normalized to 0.6, 0.4
    let fused = weighted_fusion(vec![source1, source2], &[60.0, 40.0]);

    let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
    let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

    assert!((doc_a.1.to_f64() - 0.6).abs() < 0.01);
    assert!((doc_b.1.to_f64() - 0.4).abs() < 0.01);
}

#[test]
fn test_normalization_negative_weights() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_b", 1.0)]);

    // Negative weight should be treated as 0
    let fused = weighted_fusion(vec![source1, source2], &[1.0, -0.5]);

    let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
    let doc_b = fused.iter().find(|(id, _)| *id == "doc_b");

    // doc_a gets full weight (1.0 normalized to 1.0)
    assert!((doc_a.1.to_f64() - 1.0).abs() < 0.01);
    // doc_b should have 0 contribution from second source
    assert!(doc_b.is_none() || doc_b.unwrap().1.to_f64() < 0.01);
}

#[test]
fn test_normalization_three_sources_equal() {
    let source1 = make_results(vec![("doc_a", 1.0)]);
    let source2 = make_results(vec![("doc_b", 1.0)]);
    let source3 = make_results(vec![("doc_c", 1.0)]);

    // Equal weights
    let fused = weighted_fusion(vec![source1, source2, source3], &[1.0, 1.0, 1.0]);

    for (_, score) in &fused {
        // Each should get 1/3 weight = 0.333...
        assert!((score.to_f64() - 1.0 / 3.0).abs() < 0.01);
    }
}

#[test]
fn test_normalization_consistent_across_scales() {
    let source1 = make_results(vec![("doc_a", 0.8), ("doc_b", 0.6)]);
    let source2 = make_results(vec![("doc_a", 0.9), ("doc_c", 0.7)]);

    // Same ratio, different scales
    let fused1 = weighted_fusion(vec![source1.clone(), source2.clone()], &[0.7, 0.3]);
    let fused2 = weighted_fusion(vec![source1.clone(), source2.clone()], &[7.0, 3.0]);
    let fused3 = weighted_fusion(vec![source1, source2], &[70.0, 30.0]);

    // All should produce identical results
    assert_eq!(fused1.len(), fused2.len());
    assert_eq!(fused2.len(), fused3.len());

    for i in 0..fused1.len() {
        assert_eq!(fused1[i].0, fused2[i].0);
        assert_eq!(fused2[i].0, fused3[i].0);
        assert!(
            (fused1[i].1.to_f64() - fused2[i].1.to_f64()).abs() < 1e-10,
            "Score mismatch at position {}: {} vs {}",
            i,
            fused1[i].1.to_f64(),
            fused2[i].1.to_f64()
        );
        assert!(
            (fused2[i].1.to_f64() - fused3[i].1.to_f64()).abs() < 1e-10,
            "Score mismatch at position {}: {} vs {}",
            i,
            fused2[i].1.to_f64(),
            fused3[i].1.to_f64()
        );
    }
}

// Helper function tests

#[test]
fn test_weights_are_normalized() {
    assert!(weights_are_normalized(&[0.5, 0.5], 1e-6));
    assert!(weights_are_normalized(&[0.7, 0.3], 1e-6));
    assert!(weights_are_normalized(&[1.0], 1e-6));
    assert!(weights_are_normalized(&[0.25, 0.25, 0.25, 0.25], 1e-6));

    assert!(!weights_are_normalized(&[0.5, 0.6], 1e-6)); // > 1
    assert!(!weights_are_normalized(&[0.3, 0.3], 1e-6)); // < 1
    assert!(!weights_are_normalized(&[10.0, 10.0], 1e-6)); // = 20
}

#[test]
fn test_normalize_weights() {
    let normalized = normalize_weights(&[6.0, 4.0]);
    assert!((normalized[0] - 0.6).abs() < 1e-10);
    assert!((normalized[1] - 0.4).abs() < 1e-10);

    let normalized = normalize_weights(&[1.0, 1.0, 1.0]);
    for w in &normalized {
        assert!((w - 1.0 / 3.0).abs() < 1e-10);
    }

    let normalized = normalize_weights(&[0.0, 0.0]);
    assert!((normalized[0] - 0.5).abs() < 1e-10);
    assert!((normalized[1] - 0.5).abs() < 1e-10);

    let normalized = normalize_weights(&[1.0, -1.0]);
    assert!((normalized[0] - 1.0).abs() < 1e-10);
    assert!((normalized[1] - 0.0).abs() < 1e-10);
}

#[test]
fn test_normalize_weights_empty() {
    let normalized = normalize_weights(&[]);
    assert!(normalized.is_empty());
}

#[test]
fn test_normalize_weights_non_finite() {
    let normalized = normalize_weights(&[f64::INFINITY, 1.0]);
    assert!(normalized.iter().all(|w| w.is_finite()));
    assert!((normalized[0] - 0.0).abs() < 1e-10);
    assert!((normalized[1] - 1.0).abs() < 1e-10);

    let normalized = normalize_weights(&[f64::NAN, 2.0, 2.0]);
    assert!(normalized.iter().all(|w| w.is_finite()));
    assert!((normalized[0] - 0.0).abs() < 1e-10);
    assert!((normalized[1] - 0.5).abs() < 1e-10);
    assert!((normalized[2] - 0.5).abs() < 1e-10);

    let normalized = normalize_weights(&[f64::INFINITY, f64::NAN]);
    assert!(normalized.iter().all(|w| w.is_finite()));
    assert!((normalized[0] - 0.5).abs() < 1e-10);
    assert!((normalized[1] - 0.5).abs() < 1e-10);
}

#[test]
fn test_normalize_weights_overflow_sum() {
    // Finite positive weights whose sum overflows to infinity must still normalize to a valid
    // distribution rather than collapsing to zeros (issue #1074 review).
    let normalized = normalize_weights(&[f64::MAX, f64::MAX]);
    assert!(normalized.iter().all(|w| w.is_finite()));
    assert!((normalized.iter().sum::<f64>() - 1.0).abs() < 1e-10);
    assert!((normalized[0] - 0.5).abs() < 1e-10);
    assert!((normalized[1] - 0.5).abs() < 1e-10);
}

// ── Per-source min-max normalization before fusion ─────────────────────

#[test]
fn test_weighted_fusion_mixed_scales_bm25_vs_cosine() {
    // BM25-like: unbounded scores (0..100)
    let bm25 = vec![
        ("doc_a", DeterministicScore::from_f64(80.0)),
        ("doc_b", DeterministicScore::from_f64(20.0)),
    ];
    // Cosine-like: [0,1] scores
    let cosine = vec![
        ("doc_a", DeterministicScore::from_f64(0.9)),
        ("doc_b", DeterministicScore::from_f64(0.1)),
    ];

    let result = weighted_fusion(vec![bm25, cosine], &[0.5, 0.5]);
    // Both sources agree doc_a is more relevant — it must rank first.
    assert_eq!(result[0].0, "doc_a");
    assert_eq!(result[1].0, "doc_b");
    // After normalization, doc_a should score close to 1.0 (top in both).
    assert!(result[0].1.to_f64() > 0.8);
}

#[test]
fn test_weighted_fusion_inverted_scale_normalizes_correctly() {
    // If one source has negative/inverted semantics, min-max still works.
    let src1 = vec![
        ("x", DeterministicScore::from_f64(100.0)),
        ("y", DeterministicScore::from_f64(1.0)),
    ];
    let src2 = vec![
        ("x", DeterministicScore::from_f64(0.9)),
        ("y", DeterministicScore::from_f64(0.1)),
    ];

    let result = weighted_fusion(vec![src1, src2], &[0.6, 0.4]);
    assert_eq!(result[0].0, "x");
    // x must score strictly higher than y
    assert!(result[0].1.to_f64() > result[1].1.to_f64());
}

#[test]
fn test_min_max_normalize_source_single() {
    let src = vec![("a", DeterministicScore::from_f64(42.0))];
    let out = min_max_normalize_source(src);
    assert!((out[0].1.to_f64() - 1.0).abs() < 1e-10);
}

#[test]
fn test_min_max_normalize_source_uniform() {
    let src = vec![
        ("a", DeterministicScore::from_f64(5.0)),
        ("b", DeterministicScore::from_f64(5.0)),
    ];
    let out = min_max_normalize_source(src);
    // All equal → all 1.0
    assert!((out[0].1.to_f64() - 1.0).abs() < 1e-10);
    assert!((out[1].1.to_f64() - 1.0).abs() < 1e-10);
}
