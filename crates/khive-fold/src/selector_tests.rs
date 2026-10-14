use super::*;

fn input(id: &str, size: usize, score: f32) -> SelectorInput<()> {
    SelectorInput {
        id: id.to_string(),
        content: (),
        size,
        score,
        category: None,
        information_gain: None,
        rank_score: None,
    }
}

fn input_cat(id: &str, size: usize, score: f32, cat: &str) -> SelectorInput<()> {
    SelectorInput {
        id: id.to_string(),
        content: (),
        size,
        score,
        category: Some(cat.to_string()),
        information_gain: None,
        rank_score: None,
    }
}

fn weights(min_score: f32) -> SelectorWeights {
    SelectorWeights {
        min_score,
        ..Default::default()
    }
}

#[test]
fn empty_input() {
    let inputs: Vec<SelectorInput<()>> = vec![];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert!(out.selected.is_empty());
    assert_eq!(out.total_size, 0);
    assert_eq!(out.budget, 1000);
}

#[test]
fn packs_highest_scores_first() {
    let inputs = vec![
        input("a", 100, 0.5),
        input("b", 100, 0.9),
        input("c", 100, 0.7),
    ];
    let out = GreedySelector.select(inputs, 200, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "b");
    assert_eq!(out.selected[1].id, "c");
    assert_eq!(out.total_size, 200);
}

#[test]
fn respects_budget() {
    let inputs = vec![
        input("a", 300, 0.9),
        input("b", 300, 0.8),
        input("c", 300, 0.7),
    ];
    let out = GreedySelector.select(inputs, 500, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.total_size, 300);
}

#[test]
fn filters_below_min_score() {
    let inputs = vec![
        input("a", 10, 0.8),
        input("b", 10, 0.1),
        input("c", 10, 0.5),
    ];
    let out = GreedySelector.select(inputs, 1000, &weights(0.3)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "c");
}

#[test]
fn filters_nan_and_inf() {
    let inputs = vec![
        input("nan", 10, f32::NAN),
        input("inf", 10, f32::INFINITY),
        input("neg_inf", 10, f32::NEG_INFINITY),
        input("ok", 10, 0.5),
    ];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "ok");
}

#[test]
fn tie_break_size_ascending() {
    let inputs = vec![input("big", 200, 0.5), input("small", 50, 0.5)];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert_eq!(out.selected[0].id, "small");
    assert_eq!(out.selected[1].id, "big");
}

#[test]
fn tie_break_id_ascending() {
    let inputs = vec![input("z", 100, 0.5), input("a", 100, 0.5)];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "z");
}

#[test]
fn skips_oversized_items_takes_smaller() {
    let inputs = vec![
        input("huge", 900, 0.9),
        input("small1", 40, 0.3),
        input("small2", 40, 0.2),
    ];
    let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "small1");
    assert_eq!(out.selected[1].id, "small2");
    assert_eq!(out.total_size, 80);
}

#[test]
fn zero_budget() {
    let inputs = vec![input("a", 1, 0.9)];
    let out = GreedySelector.select(inputs, 0, &weights(0.0)).unwrap();
    assert!(out.selected.is_empty());
}

#[test]
fn deterministic_across_input_order() {
    let a = vec![
        input("x", 50, 0.7),
        input("y", 50, 0.7),
        input("z", 50, 0.7),
    ];
    let b = vec![
        input("z", 50, 0.7),
        input("x", 50, 0.7),
        input("y", 50, 0.7),
    ];
    let out_a = GreedySelector.select(a, 100, &weights(0.0)).unwrap();
    let out_b = GreedySelector.select(b, 100, &weights(0.0)).unwrap();
    let ids_a: Vec<&str> = out_a.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_b: Vec<&str> = out_b.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_a, ids_b);
    assert_eq!(ids_a, vec!["x", "y"]);
}

#[test]
fn exact_budget_fit() {
    let inputs = vec![input("a", 50, 0.9), input("b", 50, 0.8)];
    let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.total_size, 100);
}

#[test]
fn category_weights_boost_preferred_category() {
    let inputs = vec![
        input_cat("a", 100, 0.9, "low"),
        input_cat("b", 100, 0.5, "high"),
    ];
    let w = SelectorWeights {
        category_weights: [("high".to_string(), 2.0f32), ("low".to_string(), 1.0f32)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 100, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "b");
}

#[test]
fn category_weights_can_push_below_min_score() {
    let inputs = vec![
        input_cat("a", 10, 0.4, "bad"),
        input_cat("b", 10, 0.8, "good"),
    ];
    let w = SelectorWeights {
        min_score: 0.3,
        category_weights: [("bad".to_string(), 0.5f32)].into_iter().collect(),
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 1000, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "b");
}

#[test]
fn diversity_bias_zero_identical_to_greedy() {
    let make = || {
        vec![
            input_cat("a", 100, 0.9, "x"),
            input_cat("b", 100, 0.8, "x"),
            input_cat("c", 100, 0.7, "y"),
        ]
    };
    let w_greedy = SelectorWeights {
        ..Default::default()
    };
    let w_bias0 = SelectorWeights {
        diversity_bias: 0.0,
        ..Default::default()
    };
    let out_g = GreedySelector.select(make(), 200, &w_greedy).unwrap();
    let out_b = GreedySelector.select(make(), 200, &w_bias0).unwrap();
    let ids_g: Vec<&str> = out_g.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_b: Vec<&str> = out_b.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_g, ids_b);
}

#[test]
fn diversity_bias_prefers_different_categories() {
    let inputs = vec![
        input_cat("a", 100, 0.9, "x"),
        input_cat("b", 100, 0.8, "x"),
        input_cat("c", 100, 0.7, "y"),
    ];
    let w = SelectorWeights {
        diversity_bias: 1.0,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 200, &w).unwrap();
    assert_eq!(out.selected.len(), 2);
    let ids: Vec<&str> = out.selected.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&"a"), "a should always be selected");
    assert!(
        ids.contains(&"c"),
        "c should be preferred over b due to diversity"
    );
}

#[test]
fn diversity_path_tie_breaks_id_ascending() {
    let inputs = vec![input("z", 1, 0.5), input("a", 1, 0.5)];
    let w = SelectorWeights {
        diversity_bias: 0.5,
        ..Default::default()
    };

    let out = GreedySelector.select(inputs, 1, &w).unwrap();

    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
}

#[test]
fn no_overflow_near_usize_max() {
    // Items with near-usize::MAX sizes must not overflow when checking budget.
    let large = usize::MAX - 1;
    let inputs = vec![
        SelectorInput {
            id: "a".to_string(),
            content: (),
            size: large,
            score: 0.9,
            category: None,
            information_gain: None,
            rank_score: None,
        },
        SelectorInput {
            id: "b".to_string(),
            content: (),
            size: 10,
            score: 0.8,
            category: None,
            information_gain: None,
            rank_score: None,
        },
    ];
    // Budget is 100 — only item "b" fits.
    let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "b");
}

#[test]
fn diversity_bias_no_categories_unaffected() {
    let inputs = vec![
        input("a", 100, 0.9),
        input("b", 100, 0.8),
        input("c", 100, 0.7),
    ];
    let w = SelectorWeights {
        diversity_bias: 1.0,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 200, &w).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "b");
}

// ── epistemic weight tests ────────────────────────────────────────────────

fn input_with_gain(id: &str, size: usize, score: f32, gain: f32) -> SelectorInput<()> {
    SelectorInput {
        id: id.to_string(),
        content: (),
        size,
        score,
        category: None,
        information_gain: Some(gain),
        rank_score: None,
    }
}

#[test]
fn epistemic_weight_zero_preserves_behavior() {
    // With epistemic_weight=0, result must be identical to the default (no epistemic).
    let make = || {
        vec![
            input_with_gain("a", 100, 0.9, 10.0),
            input_with_gain("b", 100, 0.8, 0.0),
            input_with_gain("c", 100, 0.7, 5.0),
        ]
    };
    let w_default = SelectorWeights {
        ..Default::default()
    };
    let w_zero = SelectorWeights {
        epistemic_weight: 0.0,
        ..Default::default()
    };
    let out_d = GreedySelector.select(make(), 200, &w_default).unwrap();
    let out_z = GreedySelector.select(make(), 200, &w_zero).unwrap();
    let ids_d: Vec<&str> = out_d.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_z: Vec<&str> = out_z.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_d, ids_z);
    // Pure score order: a (0.9), b (0.8).
    assert_eq!(ids_d, vec!["a", "b"]);
}

#[test]
fn epistemic_weight_positive_reorders_by_gain() {
    // a: score=0.5, gain=10.0  → effective = 0.5 + 1.0 * 10.0 = 10.5
    // b: score=0.9, gain=0.0   → effective = 0.9 + 1.0 * 0.0  = 0.9
    // With epistemic_weight=1.0, a should be selected first.
    let inputs = vec![
        input_with_gain("a", 100, 0.5, 10.0),
        input_with_gain("b", 100, 0.9, 0.0),
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 100, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
}

#[test]
fn information_gain_none_equivalent_to_zero() {
    // None and Some(0.0) must produce identical ordering.
    let with_none = vec![
        input("a", 100, 0.9), // information_gain: None
        input("b", 100, 0.8),
    ];
    let with_zero = vec![
        input_with_gain("a", 100, 0.9, 0.0),
        input_with_gain("b", 100, 0.8, 0.0),
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        ..Default::default()
    };
    let out_none = GreedySelector.select(with_none, 200, &w).unwrap();
    let out_zero = GreedySelector.select(with_zero, 200, &w).unwrap();
    let ids_none: Vec<&str> = out_none.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_zero: Vec<&str> = out_zero.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_none, ids_zero);
}

#[test]
fn epistemic_weight_works_with_diversity_bias() {
    // Combines epistemic and diversity: the effective score incorporates both.
    // a: score=0.5, gain=10.0, category=x → base effective = 0.5 + 1.0 * 10.0 = 10.5
    // b: score=0.8, gain=0.0,  category=x → base effective = 0.8
    // c: score=0.3, gain=0.0,  category=y → base effective = 0.3
    // Budget=200, bias=0.5: a selected first (10.5 wins), then after a is in x,
    // b's diversity penalty is 0.8*(1-0.5*1/2)=0.8*0.75=0.6 vs c at 0.3 — b wins.
    let inputs = vec![
        {
            let mut i = input_with_gain("a", 100, 0.5, 10.0);
            i.category = Some("x".to_string());
            i
        },
        {
            let mut i = input_with_gain("b", 100, 0.8, 0.0);
            i.category = Some("x".to_string());
            i
        },
        {
            let mut i = input_with_gain("c", 100, 0.3, 0.0);
            i.category = Some("y".to_string());
            i
        },
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        diversity_bias: 0.5,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 200, &w).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    // b (eff=0.8*0.75=0.6) > c (eff=0.3) after a is placed in category x.
    assert_eq!(out.selected[1].id, "b");
}

// ── non-finite validation tests (FOLD-AUD-003) ─────────────────────────────

#[test]
fn greedy_selector_rejects_nan_information_gain() {
    let inputs = vec![
        input_with_gain("a", 100, 0.1, f32::NAN),
        input_with_gain("b", 100, 0.9, 0.0),
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        ..Default::default()
    };
    let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
    assert!(matches!(err, FoldError::InvalidInput(_)));
}

#[test]
fn greedy_selector_rejects_non_finite_epistemic_weight() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let inputs = vec![input("a", 100, 0.5)];
        let w = SelectorWeights {
            epistemic_weight: bad,
            ..Default::default()
        };
        let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }
}

#[test]
fn greedy_selector_rejects_non_finite_diversity_bias() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let inputs = vec![input("a", 100, 0.5)];
        let w = SelectorWeights {
            diversity_bias: bad,
            ..Default::default()
        };
        let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }
}

#[test]
fn greedy_selector_rejects_out_of_range_diversity_bias() {
    for bad in [-0.1, 1.1] {
        let inputs = vec![input("a", 100, 0.5)];
        let w = SelectorWeights {
            diversity_bias: bad,
            ..Default::default()
        };

        let err = GreedySelector.select(inputs, 100, &w).unwrap_err();

        assert_eq!(
            err.to_string(),
            "invalid input: SelectorWeights.diversity_bias must be within [0.0, 1.0]"
        );
    }
}

#[test]
fn greedy_selector_rejects_non_finite_category_weight() {
    let inputs = vec![input_cat("a", 100, 0.5, "x")];
    let w = SelectorWeights {
        category_weights: [("x".to_string(), f32::NAN)].into_iter().collect(),
        ..Default::default()
    };
    let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
    assert!(matches!(err, FoldError::InvalidInput(_)));
}

#[test]
fn greedy_selector_handles_extreme_f32_products_without_overflow() {
    // f64/DeterministicScore ranking no longer overflows on f32::MAX products
    // (FOLD-AUD); see design.md#test-rationale-notes.
    let inputs = vec![input_with_gain("a", 100, f32::MAX, f32::MAX)];
    let w = SelectorWeights {
        epistemic_weight: f32::MAX,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 100, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
}

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
