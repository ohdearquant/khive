use super::*;

#[test]
fn derive_weights_sums_to_one() {
    let state = SectionPosteriorState::new();
    let mut rng = rand::rng();
    for _ in 0..20 {
        let weights = derive_weights(&state, &mut rng);
        assert_eq!(weights.len(), SectionType::all().len());
        let sum: f64 = weights.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "weights must sum to 1.0; got {sum}"
        );
    }
}

/// The same weights must normalize to bit-identical results whatever order
/// the map happens to iterate in.
#[test]
fn renormalization_does_not_depend_on_map_iteration_order() {
    let raw = [0.1, 0.2, 0.3, 0.7, 0.11, 0.13, 0.17, 0.19];
    let normalize = |order: &[usize]| {
        let mut weights: HashMap<SectionType, f64> = order
            .iter()
            .map(|&i| (SectionType::ALL[i], raw[i]))
            .collect();
        apply_floor_and_renorm(&mut weights, DEFAULT_SECTION_WEIGHT_FLOOR);
        SectionType::ALL.map(|section| weights[&section].to_bits())
    };
    let forward: Vec<usize> = (0..raw.len()).collect();
    let expected = normalize(&forward);
    for round in 0..64 {
        let mut order = forward.clone();
        order.rotate_left(round % raw.len());
        if round % 2 == 1 {
            order.reverse();
        }
        assert_eq!(normalize(&order), expected, "round {round}");
    }
}

#[test]
fn deterministic_weights_sum_to_one() {
    let state = SectionPosteriorState::new();
    let weights = derive_deterministic_weights(&state);
    assert_eq!(weights.len(), 8);
    let sum: f64 = weights.values().sum();
    assert!(
        (sum - 1.0).abs() < 1e-9,
        "deterministic weights must sum to 1.0; got {sum}"
    );
}

#[test]
fn epoch_zero_uses_deterministic() {
    let mut state = SectionPosteriorState::new();
    state.exploration_epoch = 0;
    let mut rng = rand::rng();
    let w1 = derive_weights(&state, &mut rng);
    let w2 = derive_weights(&state, &mut rng);
    for st in SectionType::all() {
        assert!(
            (w1[st] - w2[st]).abs() < 1e-12,
            "epoch=0 should give deterministic weights for {:?}",
            st
        );
    }
}

#[test]
fn higher_alpha_gets_higher_weight() {
    let mut priors = SectionPosteriorState::default_priors();
    priors.insert(
        SectionType::OperationalGuidance,
        BetaPosterior::new(20.0, 2.0),
    );
    let mut state = SectionPosteriorState::from_priors(priors);
    state.exploration_epoch = 0;
    let weights = derive_deterministic_weights(&state);
    let og = weights[&SectionType::OperationalGuidance];
    let form = weights[&SectionType::Formalism];
    assert!(og > form, "og={og:.4} form={form:.4}");
}

/// BRAIN-0483: hand-computed `lower_confidence_bound` values.
///
/// alpha=11, beta=1 (10-for-10 lucky streak): mean = 11/12 = 0.916667,
/// variance = alpha*beta / (n^2*(n+1)) = 11 / (144*13) = 0.00587607,
/// sqrt(variance) = 0.0766595, lcb = 0.916667 - 0.0766595 = 0.840007.
///
/// alpha=901, beta=101 (900-for-1000): mean = 901/1002 = 0.899202,
/// variance = 91001 / (1002^2*1003) = 91001 / 1007016012 = 0.0000903668,
/// sqrt(variance) = 0.00950614, lcb = 0.899202 - 0.00950614 = 0.889696.
#[test]
fn lower_confidence_bound_hand_computed() {
    let lucky = BetaPosterior::new(11.0, 1.0);
    let seasoned = BetaPosterior::new(901.0, 101.0);
    assert!(
        (lower_confidence_bound(&lucky) - 0.840_007).abs() < 1e-5,
        "lucky lcb = {}",
        lower_confidence_bound(&lucky)
    );
    assert!(
        (lower_confidence_bound(&seasoned) - 0.889_696).abs() < 1e-5,
        "seasoned lcb = {}",
        lower_confidence_bound(&seasoned)
    );
}

/// BRAIN-0483: a section with one lucky streak (high mean, tiny evidence)
/// must not outrank a section with hundreds of observations at a lower raw
/// mean once `deterministic_weights` is the permanent scoring path
/// (`exploration_epoch == 0`). Raw mean alone gets this backwards
/// (0.916667 > 0.899202); the lower-confidence-bound correction flips it
/// (0.840007 < 0.889696), matching the evidence.
#[test]
fn deterministic_weights_does_not_let_a_lucky_streak_beat_seasoned_evidence() {
    let mut priors = SectionPosteriorState::default_priors();
    priors.insert(SectionType::Examples, BetaPosterior::new(11.0, 1.0));
    priors.insert(
        SectionType::OperationalGuidance,
        BetaPosterior::new(901.0, 101.0),
    );
    let mut state = SectionPosteriorState::from_priors(priors);
    state.exploration_epoch = 0;

    let raw_mean_examples = state.posteriors[&SectionType::Examples].mean();
    let raw_mean_og = state.posteriors[&SectionType::OperationalGuidance].mean();
    assert!(
            raw_mean_examples > raw_mean_og,
            "fixture must reproduce the raw-mean inversion: examples={raw_mean_examples:.6} og={raw_mean_og:.6}"
        );

    let weights = derive_deterministic_weights(&state);
    let examples = weights[&SectionType::Examples];
    let og = weights[&SectionType::OperationalGuidance];
    assert!(
            og > examples,
            "seasoned evidence must outrank a lucky streak once corrected: og={og:.6} examples={examples:.6}"
        );
}
