use khive_retrieval::{calibrate_engine_hits, CalibratedHit, ErrorKind, RetrievalError};

#[test]
fn floor_and_threshold_filter_before_assigning_compact_ranks() {
    let hits = calibrate_engine_hits(
        vec![(1, -0.75), (2, -0.5), (3, -0.25), (4, 0.0), (5, 0.5)],
        -0.5,
        0.5,
        0.5,
    )
    .unwrap();
    assert_eq!(
        hits,
        vec![
            CalibratedHit {
                id: 5,
                raw_similarity: 0.5,
                normalized_similarity: 1.0,
                rank: 1,
            },
            CalibratedHit {
                id: 4,
                raw_similarity: 0.0,
                normalized_similarity: 0.5,
                rank: 2,
            },
        ]
    );
}

#[test]
fn a_zero_threshold_keeps_the_floor_but_not_scores_below_it() {
    let hits = calibrate_engine_hits(vec![(1, -0.75), (2, -0.5)], -0.5, 1.0, 0.0).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 2);
    assert_eq!(hits[0].normalized_similarity, 0.0);
    assert_eq!(hits[0].rank, 1);
}

#[test]
fn normalization_caps_preserve_distinct_raw_order_and_bits() {
    let low = 0.75_f64;
    let high = f64::from_bits(low.to_bits() + 1);
    let hits = calibrate_engine_hits(vec![(1, low), (2, high)], -1.0, 0.5, 1.0).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(hits[0].raw_similarity.to_bits(), high.to_bits());
    assert_eq!(hits[1].raw_similarity.to_bits(), low.to_bits());
    assert!(hits.iter().all(|hit| hit.normalized_similarity == 1.0));
}

#[test]
fn repeated_ids_keep_the_best_raw_score_without_rank_gaps() {
    let hits = calibrate_engine_hits(
        vec![(1, 0.25), (2, 0.5), (1, 0.75), (3, -0.5), (2, 0.5)],
        0.0,
        1.0,
        0.0,
    )
    .unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| (hit.id, hit.raw_similarity, hit.rank))
            .collect::<Vec<_>>(),
        [(1, 0.75, 1), (2, 0.5, 2)]
    );
}

#[test]
fn equal_scores_use_ids_independently_of_input_order() {
    for ids in [[3, 1, 2], [2, 3, 1], [1, 2, 3], [3, 2, 1]] {
        let hits = calibrate_engine_hits(
            ids.into_iter().map(|id| (id, 0.5)).collect(),
            -1.0,
            1.0,
            0.0,
        )
        .unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [1, 2, 3]);
    }
}

#[test]
fn signed_zeros_tie_by_id_and_duplicate_ties_keep_the_first_raw_bits() {
    for (first, second) in [(-0.0_f64, 0.0_f64), (0.0_f64, -0.0_f64)] {
        let hits =
            calibrate_engine_hits(vec![(2, second), (1, first), (1, second)], -1.0, 1.0, 0.0)
                .unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(hits[0].raw_similarity.to_bits(), first.to_bits());
        assert_eq!(hits[1].raw_similarity.to_bits(), second.to_bits());
    }
}

#[test]
fn explicit_cosine_defaults_keep_the_full_finite_cosine_range() {
    let hits = calibrate_engine_hits(vec![(1, -1.0), (2, 0.0), (3, 1.0)], -1.0, 1.0, 0.0).unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| (hit.id, hit.normalized_similarity, hit.rank))
            .collect::<Vec<_>>(),
        [(3, 1.0, 1), (2, 0.5, 2), (1, 0.0, 3)]
    );
    assert!(calibrate_engine_hits::<u32>(vec![], -1.0, 1.0, 0.0)
        .unwrap()
        .is_empty());
}

#[test]
fn invalid_bounds_fail_even_without_hits_and_are_permanent() {
    for (floor, maximum, threshold) in [
        (-2.0, 1.0, 0.0),
        (-1.0, 2.0, 0.0),
        (0.5, 0.5, 0.0),
        (0.75, 0.5, 0.0),
        (f64::NAN, 1.0, 0.0),
        (f64::NEG_INFINITY, 1.0, 0.0),
        (-1.0, f64::NAN, 0.0),
        (-1.0, f64::INFINITY, 0.0),
        (-1.0, 1.0, -0.25),
        (-1.0, 1.0, 1.25),
        (-1.0, 1.0, f64::NAN),
        (-1.0, 1.0, f64::INFINITY),
    ] {
        let error = calibrate_engine_hits::<u32>(vec![], floor, maximum, threshold).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Permanent);
        assert!(matches!(error, RetrievalError::Configuration(_)));
    }
}

#[test]
fn nonfinite_input_refuses_the_whole_list_with_its_original_position() {
    for score in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let error = calibrate_engine_hits(vec![(1, 0.5), (2, score)], 0.0, 1.0, 0.0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Permanent);
        let RetrievalError::InvalidQuery(message) = error else {
            panic!("nonfinite caller input must be InvalidQuery");
        };
        assert_eq!(
            message,
            "calibration raw similarity at input position 1 must be finite"
        );
    }
}

#[test]
fn invalid_parameters_take_precedence_over_invalid_raw_input() {
    let error = calibrate_engine_hits(vec![(1, f64::NAN)], 0.5, 0.5, 0.0).unwrap_err();
    assert!(matches!(error, RetrievalError::Configuration(_)));
}

#[test]
fn finite_values_above_the_cap_follow_the_formula_without_a_new_range_policy() {
    let hits =
        calibrate_engine_hits(vec![(1, f64::MAX), (2, 0.0)], 0.0, f64::MIN_POSITIVE, 1.0).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);
    assert_eq!(hits[0].raw_similarity.to_bits(), f64::MAX.to_bits());
    assert_eq!(hits[0].normalized_similarity, 1.0);
}
