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

include!("selector_tests/basic_selection_tests.rs");

include!("selector_tests/category_and_diversity_tests.rs");

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

include!("selector_tests/epistemic_weight_tests.rs");

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

include!("selector_tests/rank_score_and_zero_cost_tests.rs");

#[path = "selector_tests/diversity_index_tests.rs"]
mod diversity_index_tests;
