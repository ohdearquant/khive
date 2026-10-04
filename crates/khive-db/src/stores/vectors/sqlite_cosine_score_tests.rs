use super::*;

#[test]
fn canonical_conversion_keeps_opposite_vectors_negative() {
    assert_eq!(
        sqlite_cosine_score(2.0).unwrap(),
        DeterministicScore::from_f64(-1.0)
    );
}

#[test]
fn canonical_conversion_normalizes_only_endpoint_roundoff() {
    assert_eq!(
        sqlite_cosine_score(-f64::EPSILON).unwrap(),
        DeterministicScore::from_f64(1.0)
    );
    assert_eq!(
        sqlite_cosine_score(2.0 + f64::EPSILON).unwrap(),
        DeterministicScore::from_f64(-1.0)
    );
    // f32-scale roundoff, the magnitude sqlite-vec's own f32 accumulation
    // actually produces for a non-trivial self- or opposite-comparison.
    assert_eq!(
        sqlite_cosine_score(-(f32::EPSILON as f64)).unwrap(),
        DeterministicScore::from_f64(1.0)
    );
    assert_eq!(
        sqlite_cosine_score(2.0 + f32::EPSILON as f64).unwrap(),
        DeterministicScore::from_f64(-1.0)
    );
}

#[test]
fn canonical_conversion_rejects_invalid_driver_distances() {
    for distance in [
        f64::NAN,
        f64::INFINITY,
        -16.0 * f32::EPSILON as f64,
        2.0 + 16.0 * f32::EPSILON as f64,
        -0.1,
        2.1,
    ] {
        assert!(
            sqlite_cosine_score(distance).is_err(),
            "distance {distance:?} must fail strict canonical validation"
        );
    }
}
