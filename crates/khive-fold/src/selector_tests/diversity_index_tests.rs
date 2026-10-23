use super::*;

#[path = "diversity_reference.rs"]
mod reference;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Payload {
    ordinal: usize,
    text: String,
    bytes: Vec<u8>,
}

fn candidate(
    ordinal: usize,
    id: &str,
    size: usize,
    rank: f64,
    category: Option<&str>,
) -> SelectorInput<Payload> {
    SelectorInput {
        id: id.to_string(),
        content: Payload {
            ordinal,
            text: format!("payload-{ordinal}"),
            bytes: ordinal.to_le_bytes().to_vec(),
        },
        size,
        score: 1.0,
        category: category.map(str::to_string),
        information_gain: None,
        rank_score: Some(rank),
    }
}

fn compare_with_reference(
    inputs: Vec<SelectorInput<Payload>>,
    budget: usize,
    weights: &SelectorWeights,
    label: &str,
) -> Option<SelectorOutput<Payload>> {
    let expected = reference::ReferenceSelector.select(inputs.clone(), budget, weights);
    let actual = GreedySelector.select(inputs, budget, weights);
    match (actual, expected) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(actual.budget, expected.budget, "{label}");
            assert_eq!(actual.total_size, expected.total_size, "{label}");
            assert_eq!(actual.selected.len(), expected.selected.len(), "{label}");
            for (index, (actual, expected)) in
                actual.selected.iter().zip(&expected.selected).enumerate()
            {
                assert_eq!(actual.content, expected.content, "{label}, slot {index}");
                assert_eq!(actual.id, expected.id, "{label}, slot {index}");
                assert_eq!(actual.size, expected.size, "{label}, slot {index}");
                assert_eq!(actual.category, expected.category, "{label}, slot {index}");
                assert_eq!(
                    actual.score.to_bits(),
                    expected.score.to_bits(),
                    "{label}, slot {index}"
                );
                assert_eq!(
                    actual.rank_score.map(f64::to_bits),
                    expected.rank_score.map(f64::to_bits),
                    "{label}, slot {index}"
                );
                assert_eq!(
                    actual.information_gain.map(f32::to_bits),
                    expected.information_gain.map(f32::to_bits),
                    "{label}, slot {index}"
                );
            }
            Some(actual)
        }
        (Err(actual), Err(expected)) => {
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{label}");
            None
        }
        (actual, expected) => panic!("{label}: actual={actual:?}, expected={expected:?}"),
    }
}

fn assert_order(inputs: Vec<SelectorInput<Payload>>, budget: usize, bias: f32, expected: &[usize]) {
    let weights = SelectorWeights {
        min_score: -f32::MAX,
        diversity_bias: bias,
        ..Default::default()
    };
    let output = compare_with_reference(inputs, budget, &weights, "focused case").unwrap();
    let order: Vec<_> = output
        .selected
        .iter()
        .map(|item| item.content.ordinal)
        .collect();
    assert_eq!(order, expected);
}

#[test]
fn quantized_penalty_plateau_uses_size_before_raw_base() {
    let quantum = 1.0 / 4_294_967_296.0;
    assert_order(
        vec![
            candidate(0, "seed", 0, 3.0, Some("x")),
            candidate(1, "a", 2, 1.0 + 0.8 * quantum, Some("x")),
            candidate(2, "z", 1, 1.0, Some("x")),
        ],
        3,
        1.0,
        &[0, 2, 1],
    );
    assert_order(
        vec![
            candidate(0, "seed", 0, 3.0, Some("x")),
            candidate(1, "z", 1, 1.0 + 0.8 * quantum, Some("x")),
            candidate(2, "a", 1, 1.0, Some("x")),
        ],
        2,
        1.0,
        &[0, 2, 1],
    );
}

#[test]
fn oversized_category_leader_does_not_hide_fitting_candidate() {
    assert_order(
        vec![
            candidate(0, "large", usize::MAX, 5.0, Some("x")),
            candidate(1, "small", 1, 3.0, Some("x")),
            candidate(2, "other", 1, 2.0, Some("y")),
        ],
        1,
        0.5,
        &[1],
    );
}

#[test]
fn duplicate_ties_follow_current_swap_remove_positions() {
    for categories in [
        [None, None, None],
        [Some("x"), Some("x"), Some("x")],
        [Some("x"), Some("y"), Some("z")],
    ] {
        assert_order(
            vec![
                candidate(0, "seed", 0, 4.0, Some("seed")),
                candidate(1, "duplicate", 1, 1.0, categories[0]),
                candidate(2, "duplicate", 1, 1.0, categories[1]),
                candidate(3, "duplicate", 1, 1.0, categories[2]),
            ],
            3,
            0.5,
            &[0, 2, 1, 3],
        );
    }
}

#[test]
fn oversized_candidate_keeps_its_swap_remove_slot() {
    assert_order(
        vec![
            candidate(0, "seed", 1, 4.0, None),
            candidate(1, "duplicate", 1, 1.0, None),
            candidate(2, "duplicate", 1, 1.0, None),
            candidate(3, "oversized", usize::MAX, 0.5, None),
        ],
        3,
        0.5,
        &[0, 2, 1],
    );
}

#[test]
fn category_refresh_can_raise_a_negative_effective_score() {
    assert_order(
        vec![
            candidate(0, "seed", 0, 4.0, Some("x")),
            candidate(1, "negative-x", 1, -1.0, Some("x")),
            candidate(2, "negative-y", 1, -0.75, Some("y")),
        ],
        2,
        1.0,
        &[0, 1, 2],
    );
}

#[test]
fn missing_category_never_accumulates_a_diversity_count() {
    assert_order(
        vec![
            candidate(0, "first", 1, 4.0, None),
            candidate(1, "second", 1, 1.0, None),
            candidate(2, "other", 1, 0.75, Some("x")),
        ],
        3,
        1.0,
        &[0, 1, 2],
    );
}

#[test]
fn saturation_signed_zero_and_subnormal_plateaus_match_reference() {
    let ranks = [
        f64::MAX,
        f64::MAX / 2.0,
        2_147_483_648.0,
        1.0 / 4_294_967_296.0,
        f64::from_bits(1),
        0.0,
        -0.0,
        -f64::from_bits(1),
        -1.0 / 4_294_967_296.0,
        -2_147_483_648.0,
        -f64::MAX / 2.0,
        -f64::MAX,
    ];
    for bias in [f32::from_bits(1), 0.5, 1.0] {
        let inputs = ranks
            .into_iter()
            .enumerate()
            .map(|(i, rank)| candidate(i, "duplicate", i % 3, rank, Some("x")))
            .collect();
        assert!(compare_with_reference(
            inputs,
            usize::MAX,
            &SelectorWeights {
                diversity_bias: bias,
                ..Default::default()
            },
            "saturation and signed zero",
        )
        .is_some());
    }
}

#[test]
fn effective_score_validation_precedes_budget_rejection() {
    let weights = SelectorWeights {
        diversity_bias: 0.5,
        category_weights: [("x".to_string(), 2.0)].into_iter().collect(),
        ..Default::default()
    };
    assert!(compare_with_reference(
        vec![candidate(0, "invalid", usize::MAX, f64::MAX, Some("x"))],
        0,
        &weights,
        "non-finite weighted rank before budget",
    )
    .is_none());
}

fn next_random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

#[test]
fn seeded_diversity_selection_matches_frozen_reference() {
    const CASES: usize = 1024;
    let quantum = 1.0 / 4_294_967_296.0;
    let ranks = [
        -f64::MAX,
        -2_147_483_648.0,
        -2.0,
        -quantum,
        -f64::from_bits(1),
        -0.0,
        0.0,
        f64::from_bits(1),
        0.4 * quantum,
        0.8 * quantum,
        1.0,
        1.0 + 0.4 * quantum,
        1.0 + 0.8 * quantum,
        2_147_483_648.0,
        f64::MAX / 2.0,
        f64::MAX,
    ];
    let scores = [
        -f32::MAX,
        -2.0,
        -0.0,
        0.0,
        f32::from_bits(1),
        0.5,
        1.0,
        f32::MAX,
    ];
    let gains = [
        -f32::MAX,
        -2.0,
        -0.0,
        0.0,
        f32::from_bits(1),
        0.5,
        1.0,
        f32::MAX,
    ];
    let category_weights = [-1.0, -0.0, 0.0, 0.5, 1.0, 2.0, f32::MAX];
    let mut successes = 0;
    let mut errors = 0;
    for case in 0..CASES {
        let mut state = 0x5799_cafe_1020_u64 ^ (case as u64).wrapping_mul(0x9e37_79b9);
        let count = case % 81;
        let mut inputs = Vec::with_capacity(count);
        for ordinal in 0..count {
            let category = match case % 6 {
                0 => None,
                1 => Some("one".to_string()),
                2 => Some(format!("cat-{}", next_random(&mut state) % 8)),
                3 => Some(format!("cat-{ordinal}")),
                4 => (ordinal % 3 != 0).then(|| format!("cat-{}", ordinal % 4)),
                _ => Some(format!("cat-{}", ordinal % 2)),
            };
            let size = match next_random(&mut state) % 8 {
                0 | 1 => 0,
                2 => usize::MAX,
                3 => usize::MAX - 1,
                _ => (next_random(&mut state) % 17) as usize,
            };
            let id = format!("id-{}", next_random(&mut state) % 5);
            let mut item = candidate(ordinal, &id, size, 0.0, category.as_deref());
            item.score = scores[(next_random(&mut state) % scores.len() as u64) as usize];
            item.rank_score = (!next_random(&mut state).is_multiple_of(4))
                .then(|| ranks[(next_random(&mut state) % ranks.len() as u64) as usize]);
            item.information_gain = (!next_random(&mut state).is_multiple_of(3))
                .then(|| gains[(next_random(&mut state) % gains.len() as u64) as usize]);
            inputs.push(item);
        }
        let mut weights = SelectorWeights {
            min_score: [-f32::MAX, -1.0, 0.0, 0.5][case % 4],
            diversity_bias: [f32::from_bits(1), 0.25, 0.5, 1.0, 0.0][case % 5],
            epistemic_weight: [0.0, -1.0, 0.5, 1.0, f32::MAX][case % 5],
            ..Default::default()
        };
        if case % 3 != 0 {
            for category in ["one", "cat-0", "cat-1", "cat-3"] {
                let weight = category_weights
                    [(next_random(&mut state) % category_weights.len() as u64) as usize];
                let weight = if case % 4 == 3 {
                    weight
                } else {
                    weight.min(1.0)
                };
                weights
                    .category_weights
                    .insert(category.to_string(), weight);
            }
        }
        if case % 64 == 63 {
            inputs.push(candidate(
                count,
                "overflow",
                usize::MAX,
                f64::MAX,
                Some("overflow"),
            ));
            weights.category_weights.insert("overflow".to_string(), 2.0);
        }
        let budget = match case % 6 {
            0 => 0,
            1 => 1,
            2 => count,
            3 => count * 7,
            4 => usize::MAX,
            _ => usize::MAX - 1,
        };
        if compare_with_reference(inputs, budget, &weights, &format!("seeded case {case}"))
            .is_some()
        {
            successes += 1;
        } else {
            errors += 1;
        }
    }
    assert!(
        successes >= CASES * 3 / 4,
        "the corpus must exercise successful selection"
    );
    assert!(
        errors > 0,
        "the corpus must exercise exact validation errors"
    );
}

#[test]
fn indexed_diversity_work_stays_within_amortized_bound() {
    for count in [1000usize, 5000] {
        for shape in 0..2 {
            let inputs: Vec<_> = (0..count)
                .map(|ordinal| {
                    let category = format!("cat-{}", ordinal % 8);
                    let (size, rank) = if shape == 0 {
                        (
                            ordinal % 50 + 1,
                            (ordinal as f64 - count as f64 / 2.0) / count as f64,
                        )
                    } else {
                        let size = match ordinal % 4 {
                            0 => usize::MAX,
                            1 | 2 => 0,
                            _ => 3,
                        };
                        (size, 1.0 + (ordinal % 3) as f64 * 0.2 / 4_294_967_296.0)
                    };
                    candidate(ordinal, "duplicate", size, rank, Some(&category))
                })
                .collect();
            let budget = count * 32;
            let expected_size: usize = inputs
                .iter()
                .filter(|item| item.size <= budget)
                .map(|item| item.size)
                .sum();
            let expected_count = inputs.iter().filter(|item| item.size <= budget).count();
            super::super::diversity::take_work();
            let output = GreedySelector
                .select(
                    inputs.clone(),
                    budget,
                    &SelectorWeights {
                        diversity_bias: 0.5,
                        ..Default::default()
                    },
                )
                .unwrap();
            let work = super::super::diversity::take_work();
            let log = usize::BITS as usize - (count - 1).leading_zeros() as usize;
            let bound = 64 * count * (log + 1);
            assert!(
                work > 0 && work <= bound,
                "count={count}, shape={shape}, work={work}, bound={bound}"
            );
            assert_eq!(output.budget, budget);
            assert_eq!(output.total_size, expected_size);
            assert_eq!(output.selected.len(), expected_count);
            let mut seen = std::collections::BTreeSet::new();
            for item in output.selected {
                assert!(seen.insert(item.content.ordinal));
                let expected = &inputs[item.content.ordinal];
                assert_eq!(item.content, expected.content);
                assert_eq!(item.id, expected.id);
                assert_eq!(item.size, expected.size);
                assert_eq!(item.category, expected.category);
                assert_eq!(item.score.to_bits(), expected.score.to_bits());
                assert_eq!(
                    item.rank_score.map(f64::to_bits),
                    expected.rank_score.map(f64::to_bits)
                );
                assert_eq!(
                    item.information_gain.map(f32::to_bits),
                    expected.information_gain.map(f32::to_bits)
                );
            }
        }
    }
}
