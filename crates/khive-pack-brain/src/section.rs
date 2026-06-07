//! Section weight derivation — Thompson sampling and deterministic fallback.

use std::collections::HashMap;

use rand::Rng;

use crate::state::{SectionPosteriorState, SectionType};

/// Derive section weights from a SectionPosteriorState.
///
/// Dispatches between Thompson sampling (exploration_epoch > 0) and
/// deterministic mean-based weights (exploration_epoch == 0).
pub fn derive_weights(
    state: &SectionPosteriorState,
    rng: &mut impl Rng,
) -> HashMap<SectionType, f64> {
    state.weights(rng)
}

/// Derive deterministic weights from posterior means with weight floor.
pub fn derive_deterministic_weights(state: &SectionPosteriorState) -> HashMap<SectionType, f64> {
    state.deterministic_weights()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SectionPosteriorState;

    #[test]
    fn derive_weights_sums_to_one() {
        let state = SectionPosteriorState::new();
        let mut rng = rand::thread_rng();
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

    #[test]
    fn derive_weights_all_positive() {
        let state = SectionPosteriorState::new();
        let mut rng = rand::thread_rng();
        let weights = derive_weights(&state, &mut rng);
        for (section, &w) in &weights {
            assert!(
                w >= 0.0,
                "weight for {section:?} must be non-negative; got {w}"
            );
        }
    }

    #[test]
    fn deterministic_weights_sum_to_one() {
        let state = SectionPosteriorState::new();
        let weights = derive_deterministic_weights(&state);
        assert_eq!(weights.len(), 10);
        let sum: f64 = weights.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "deterministic weights must sum to 1.0; got {sum}"
        );
    }

    #[test]
    fn deterministic_weights_respect_floor() {
        let state = SectionPosteriorState::new();
        let weights = derive_deterministic_weights(&state);
        let min_weight = weights.values().cloned().fold(f64::INFINITY, f64::min);
        // After normalization, the floor of 0.05 applied pre-normalization
        // means the minimum weight should be > 0.
        assert!(
            min_weight > 0.0,
            "minimum weight must be positive; got {min_weight}"
        );
    }

    #[test]
    fn epoch_zero_uses_deterministic() {
        let mut state = SectionPosteriorState::new();
        state.exploration_epoch = 0;
        let mut rng = rand::thread_rng();

        // With epoch=0, two calls should give identical weights
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
    fn higher_alpha_gets_higher_average_weight() {
        use crate::state::BetaPosterior;
        let mut priors = SectionPosteriorState::default_priors();
        priors.insert(
            SectionType::OperationalGuidance,
            BetaPosterior::new(20.0, 2.0),
        );
        let mut state = SectionPosteriorState::from_priors(priors);
        state.exploration_epoch = 0; // deterministic for consistent test

        let weights = derive_deterministic_weights(&state);
        let og_weight = weights[&SectionType::OperationalGuidance];
        let form_weight = weights[&SectionType::Formalism];
        assert!(
            og_weight > form_weight,
            "OperationalGuidance with high alpha must get higher weight; og={og_weight:.4} form={form_weight:.4}"
        );
    }
}
