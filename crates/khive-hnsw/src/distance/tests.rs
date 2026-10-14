use super::*;

// =========================================================================
// RETRIEVAL-M3: Cosine distance clamping tests
// =========================================================================

/// Verify that cosine_distance_from_parts clamps correctly when fp rounding
/// produces a dot/norm ratio slightly outside [-1, 1].
#[test]
fn test_cosine_distance_from_parts_clamping() {
    // Normal case: identical direction => distance 0
    assert!((cosine_distance_from_parts(1.0, 1.0, 1.0) - 0.0).abs() < 1e-6);

    // Normal case: opposite direction => distance 2
    assert!((cosine_distance_from_parts(-1.0, 1.0, 1.0) - 2.0).abs() < 1e-6);

    // Rounding artifact: dot slightly > denom (should clamp cosine to 1.0 => dist 0)
    let dist = cosine_distance_from_parts(1.0000002, 1.0, 1.0);
    assert!(
        dist >= 0.0,
        "distance must be non-negative after clamping, got {dist}"
    );
    assert!(
        dist <= 2.0,
        "distance must be <= 2 after clamping, got {dist}"
    );

    // Zero norms => fallback 1.0
    assert_eq!(cosine_distance_from_parts(0.0, 0.0, 1.0), 1.0);
    assert_eq!(cosine_distance_from_parts(0.0, 1.0, 0.0), 1.0);

    // Infinite denom => fallback 1.0
    assert_eq!(
        cosine_distance_from_parts(f32::NAN, f32::INFINITY, 1.0),
        1.0
    );
}

#[test]
fn test_cosine_distance() {
    let a = vec![1.0, 0.0];
    let b = vec![1.0, 0.0];
    let a_norm = 1.0;
    let b_norm = 1.0;

    let dist = compute_distance(&a, a_norm, &b, b_norm, DistanceMetric::Cosine);
    assert!(dist.abs() < 0.001); // Same vector = 0 distance

    let c = vec![0.0, 1.0];
    let dist = compute_distance(&a, a_norm, &c, 1.0, DistanceMetric::Cosine);
    assert!((dist - 1.0).abs() < 0.001); // Orthogonal = 1 distance
}

#[test]
fn test_euclidean_distance() {
    let a = vec![0.0, 0.0];
    let b = vec![3.0, 4.0];

    let dist = compute_distance(&a, 0.0, &b, 5.0, DistanceMetric::L2);
    assert!((dist - 5.0).abs() < 0.001);
}

#[test]
fn test_dot_product_distance() {
    let a = vec![1.0, 2.0];
    let b = vec![2.0, 3.0];

    let dist = compute_distance(&a, 0.0, &b, 0.0, DistanceMetric::Dot);
    // dot = 1*2 + 2*3 = 8, distance = -8
    assert!((dist - (-8.0)).abs() < 0.001);
}

#[test]
fn test_score_from_distance() {
    use khive_score::DeterministicScore;
    // f32 input loses precision on widening to f64; use 1e-6 tolerance.
    // Cosine: similarity = 1 - distance
    assert!((score_from_distance(0.2, DistanceMetric::Cosine).to_f64() - 0.8).abs() < 1e-6);

    // Dot: similarity = -distance
    assert!((score_from_distance(-5.0, DistanceMetric::Dot).to_f64() - 5.0).abs() < 1e-6);

    // Euclidean: similarity = 1/(1+distance)
    assert!((score_from_distance(1.0, DistanceMetric::L2).to_f64() - 0.5).abs() < 1e-6);

    // NaN input: score_from_distance_lossy maps invalid distance to NEG_INF
    // (ranks last rather than getting a spurious perfect score).
    assert_eq!(
        score_from_distance(f32::NAN, DistanceMetric::Cosine),
        DeterministicScore::NEG_INF
    );
}

#[test]
fn test_ordered_f32() {
    let a = OrderedF32(1.0);
    let b = OrderedF32(2.0);
    assert!(a < b);
    assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
}

// =========================================================================
// RETRIEVAL-02: NaN Handling Tests
// =========================================================================

#[test]
fn test_ordered_f32_nan_handling() {
    let nan = OrderedF32(f32::NAN);
    let finite = OrderedF32(1.0);
    let infinity = OrderedF32(f32::INFINITY);
    let neg_infinity = OrderedF32(f32::NEG_INFINITY);

    // NaN is greater than all finite values
    assert!(nan > finite);
    assert!(finite < nan);

    // NaN is greater than infinity
    assert!(nan > infinity);
    assert!(infinity < nan);

    // NaN is greater than negative infinity
    assert!(nan > neg_infinity);

    // Two NaNs are equal
    let nan2 = OrderedF32(f32::NAN);
    assert_eq!(nan.cmp(&nan2), std::cmp::Ordering::Equal);
}

#[test]
fn test_ordered_f32_sorting_with_nan() {
    // When sorting distances for nearest neighbor search, NaN should end up last
    let mut distances = [
        OrderedF32(0.5),
        OrderedF32(f32::NAN),
        OrderedF32(0.1),
        OrderedF32(0.9),
        OrderedF32(f32::NAN),
        OrderedF32(0.3),
    ];

    // Sort ascending (for min-heap behavior)
    distances.sort();

    // Non-NaN values should be at the front, sorted
    assert_eq!(distances[0].0, 0.1);
    assert_eq!(distances[1].0, 0.3);
    assert_eq!(distances[2].0, 0.5);
    assert_eq!(distances[3].0, 0.9);
    // NaN values should be at the end
    assert!(distances[4].is_nan());
    assert!(distances[5].is_nan());
}

#[test]
fn test_ordered_f32_deterministic_ordering() {
    // Same values should always produce same ordering
    for _ in 0..10 {
        let values = vec![OrderedF32(0.5), OrderedF32(0.5), OrderedF32(0.1)];

        let mut sorted = values.clone();
        sorted.sort();

        assert_eq!(sorted[0].0, 0.1);
        assert_eq!(sorted[1].0, 0.5);
        assert_eq!(sorted[2].0, 0.5);
    }
}

#[test]
fn test_ordered_f32_infinity() {
    let a = OrderedF32(f32::INFINITY);
    let b = OrderedF32(f32::NEG_INFINITY);
    let c = OrderedF32(0.0);

    assert!(a > c);
    assert!(b < c);
    assert!(a > b);
}
