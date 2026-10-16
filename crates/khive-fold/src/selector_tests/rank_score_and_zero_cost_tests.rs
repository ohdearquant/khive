// ── DeterministicScore rank-comparator tests (fold PR #535) ──

#[test]
fn rejects_non_finite_rank_score() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut item = input("a", 100, 0.5);
        item.rank_score = Some(bad);
        let err = GreedySelector
            .select(vec![item], 100, &weights(0.0))
            .unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }
}

#[test]
fn rank_score_saturates_at_deterministic_score_max_without_panic() {
    // rank_score beyond DeterministicScore's range saturates to MAX rather
    // than panicking; see design.md#test-rationale-notes.
    let mut big = input("big", 200, 0.0);
    big.rank_score = Some(f64::MAX);
    let mut small = input("small", 50, 0.0);
    small.rank_score = Some(f64::MAX / 2.0);

    let out = GreedySelector
        .select(vec![big, small], 1000, &weights(0.0))
        .unwrap();
    assert_eq!(out.selected.len(), 2);
    // Both saturate to DeterministicScore::MAX and tie; size-ascending wins.
    assert_eq!(out.selected[0].id, "small");
    assert_eq!(out.selected[1].id, "big");
}

#[test]
fn rank_score_distinguishes_values_within_f32_ulp_of_one() {
    // 1.0 vs 1.00000004 collapse to identical f32 bits but differ at
    // khive-score's fixed-point scale; see design.md#test-rationale-notes.
    let a_score = 1.0_f32;
    let b_score = 1.0_f32; // identical f32 bits to a after narrowing
    assert_eq!(a_score.to_bits(), b_score.to_bits());

    let mut a = input("a", 100, a_score);
    a.rank_score = Some(1.0);
    let mut b = input("b", 100, b_score);
    b.rank_score = Some(1.000_000_04);

    let out = GreedySelector
        .select(vec![a, b], 100, &weights(0.0))
        .unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(
        out.selected[0].id, "b",
        "higher rank_score must win despite tied f32 score"
    );
}

#[test]
fn category_weights_reorder_candidates_when_rank_score_present() {
    // Regression for PR #535: rank_score must be scaled by category
    // weight too, or "a" wins despite "high"'s 2.0x weight.
    let mut a = input_cat("a", 100, 0.9, "low");
    a.rank_score = Some(0.9);
    let mut b = input_cat("b", 100, 0.5, "high");
    b.rank_score = Some(0.5);

    let w = SelectorWeights {
        category_weights: [("high".to_string(), 2.0f32), ("low".to_string(), 1.0f32)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = GreedySelector.select(vec![a, b], 100, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(
        out.selected[0].id, "b",
        "category weight must still reorder candidates when rank_score is present"
    );
}

#[test]
fn rank_score_zero_ties_break_deterministically() {
    let mut a = input("z", 100, 0.0);
    a.rank_score = Some(0.0);
    let mut b = input("a", 100, 0.0);
    b.rank_score = Some(0.0);

    let out = GreedySelector
        .select(vec![a, b], 1000, &weights(0.0))
        .unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "z");
}

#[test]
fn zero_size_candidates_fit_zero_and_exhausted_budgets() {
    for budget in [0, 1] {
        for bias in [0.0, 0.5, 1.0] {
            for reverse in [false, true] {
                let mut inputs = vec![
                    input("oversized", 2, 5.0),
                    input("paid", 1, 4.0),
                    input("free_z", 0, 2.0),
                    input("free_a", 0, 2.0),
                    input("below_floor", 0, -1.0),
                ];
                if reverse {
                    inputs.reverse();
                }
                let weights = SelectorWeights {
                    diversity_bias: bias,
                    ..Default::default()
                };
                let out = GreedySelector.select(inputs, budget, &weights).unwrap();
                let ids: Vec<_> = out.selected.iter().map(|item| item.id.as_str()).collect();
                let expected = if budget == 0 {
                    vec!["free_a", "free_z"]
                } else {
                    vec!["paid", "free_a", "free_z"]
                };
                assert_eq!(
                    ids, expected,
                    "budget={budget}, bias={bias}, reverse={reverse}"
                );
                assert_eq!(out.total_size, budget);
                assert_eq!(out.budget, budget);
            }
        }
    }
}

#[test]
fn zero_size_candidates_keep_diversity_order_at_full_budget() {
    for budget in [0, 1] {
        for (bias, expected) in [
            (0.0, ["first", "same_category", "other_category"]),
            (0.5, ["first", "other_category", "same_category"]),
            (1.0, ["first", "other_category", "same_category"]),
        ] {
            for reverse in [false, true] {
                let mut inputs = vec![
                    input_cat("oversized", budget + 1, 5.0, "b"),
                    input_cat("first", budget, 4.0, "a"),
                    input_cat("same_category", 0, 3.0, "a"),
                    input_cat("other_category", 0, 2.5, "b"),
                ];
                if reverse {
                    inputs.reverse();
                }
                let weights = SelectorWeights {
                    diversity_bias: bias,
                    ..Default::default()
                };
                let out = GreedySelector.select(inputs, budget, &weights).unwrap();
                let ids: Vec<_> = out.selected.iter().map(|item| item.id.as_str()).collect();
                assert_eq!(
                    ids, expected,
                    "budget={budget}, bias={bias}, reverse={reverse}"
                );
                assert_eq!(out.total_size, budget);
                assert_eq!(out.budget, budget);
            }
        }
    }
}
