//! Section posterior state — Thompson sampling and deterministic weight derivation.

use std::collections::HashMap;

use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::brain_signal::BrainSignal;
use crate::posterior::BetaPosterior;
use crate::section_type::SectionType;
use crate::signal::FeedbackSignal;

pub const DEFAULT_ESS_CAP: f64 = 100.0;
pub const DEFAULT_EXPLORATION_EPOCH: u64 = 50;
pub const DEFAULT_TAU_0: f64 = 1.0;
pub const DEFAULT_TAU_EXPLOIT: f64 = 0.1;
pub const DEFAULT_SECTION_WEIGHT_FLOOR: f64 = 0.05;

/// Per-profile section posterior state.
pub struct SectionPosteriorState {
    pub posteriors: HashMap<SectionType, BetaPosterior>,
    pub priors: HashMap<SectionType, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl SectionPosteriorState {
    pub fn new() -> Self {
        let priors = Self::default_priors();
        let posteriors = priors.clone();
        Self {
            posteriors,
            priors,
            total_events: 0,
            exploration_epoch: DEFAULT_EXPLORATION_EPOCH,
        }
    }

    pub fn from_priors(mut priors: HashMap<SectionType, BetaPosterior>) -> Self {
        let neutral = BetaPosterior::new(2.0, 2.0);
        for &st in SectionType::all() {
            priors.entry(st).or_insert_with(|| neutral.clone());
        }
        let posteriors = priors.clone();
        Self {
            posteriors,
            priors,
            total_events: 0,
            exploration_epoch: DEFAULT_EXPLORATION_EPOCH,
        }
    }

    pub fn default_priors() -> HashMap<SectionType, BetaPosterior> {
        let mut m = HashMap::new();
        m.insert(SectionType::Overview, BetaPosterior::new(2.0, 2.0));
        m.insert(SectionType::CoreModel, BetaPosterior::new(4.0, 2.0));
        m.insert(
            SectionType::BoundaryConditions,
            BetaPosterior::new(2.0, 3.0),
        );
        m.insert(SectionType::Formalism, BetaPosterior::new(1.5, 4.0));
        m.insert(
            SectionType::OperationalGuidance,
            BetaPosterior::new(6.0, 1.5),
        );
        m.insert(SectionType::Examples, BetaPosterior::new(5.0, 2.0));
        m.insert(SectionType::FailureModes, BetaPosterior::new(3.0, 2.0));
        m.insert(SectionType::ExpertLens, BetaPosterior::new(3.0, 2.0));
        m.insert(SectionType::References, BetaPosterior::new(2.0, 2.0));
        m.insert(SectionType::Other, BetaPosterior::new(2.0, 2.0));
        m
    }

    pub fn to_snapshot(&self) -> SectionPosteriorSnapshot {
        SectionPosteriorSnapshot {
            posteriors: self.posteriors.clone(),
            priors: self.priors.clone(),
            total_events: self.total_events,
            exploration_epoch: self.exploration_epoch,
        }
    }

    pub fn from_snapshot(snapshot: SectionPosteriorSnapshot) -> Self {
        Self {
            posteriors: snapshot.posteriors,
            priors: snapshot.priors,
            total_events: snapshot.total_events,
            exploration_epoch: snapshot.exploration_epoch,
        }
    }

    pub fn weights(&self, rng: &mut impl Rng) -> HashMap<SectionType, f64> {
        if self.exploration_epoch > 0 {
            self.sample_weights(rng)
        } else {
            self.deterministic_weights()
        }
    }

    pub fn sample_weights(&self, rng: &mut impl Rng) -> HashMap<SectionType, f64> {
        let tau =
            DEFAULT_TAU_0 * (self.exploration_epoch as f64 / DEFAULT_EXPLORATION_EPOCH as f64);
        let tau = tau.max(1e-6);

        let mut samples: HashMap<SectionType, f64> = HashMap::new();
        for (&st, posterior) in &self.posteriors {
            let theta = sample_beta_gamma(posterior.alpha.max(1e-6), posterior.beta.max(1e-6), rng);
            samples.insert(st, theta / tau);
        }

        let max_val = samples.values().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut raw: HashMap<SectionType, f64> = HashMap::new();
        for (&st, &logit) in &samples {
            raw.insert(st, (logit - max_val).exp());
        }

        apply_floor_and_renorm(&mut raw, DEFAULT_SECTION_WEIGHT_FLOOR);
        raw
    }

    pub fn deterministic_weights(&self) -> HashMap<SectionType, f64> {
        let tau = DEFAULT_TAU_EXPLOIT;

        let logits: HashMap<SectionType, f64> = self
            .posteriors
            .iter()
            .map(|(&st, p)| (st, p.mean() / tau))
            .collect();
        let max_val = logits.values().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut raw: HashMap<SectionType, f64> = HashMap::new();
        for (&st, &logit) in &logits {
            raw.insert(st, (logit - max_val).exp());
        }

        apply_floor_and_renorm(&mut raw, DEFAULT_SECTION_WEIGHT_FLOOR);
        raw
    }

    pub fn reset_posteriors(&mut self) {
        self.posteriors = self.priors.clone();
    }

    /// Apply a brain signal to update section posteriors in place.
    /// Only `Feedback` events with `section_signals` affect section state.
    pub fn apply_signal(&mut self, signal: &BrainSignal) {
        if let BrainSignal::Feedback {
            section_signals: Some(ref signals),
            ..
        } = signal
        {
            self.total_events += 1;

            for (section_type, feedback_signal) in signals {
                if let Some(posterior) = self.posteriors.get_mut(section_type) {
                    match feedback_signal {
                        FeedbackSignal::Useful => posterior.alpha += 1.0,
                        FeedbackSignal::NotUseful => posterior.beta += 1.0,
                        FeedbackSignal::Wrong => posterior.beta += 2.0,
                    }
                    if let Some(prior) = self.priors.get(section_type) {
                        posterior.apply_ess_cap(&prior.clone(), DEFAULT_ESS_CAP);
                    }
                }
            }

            if self.exploration_epoch > 0 {
                self.exploration_epoch -= 1;
            }
        }
    }
}

impl Default for SectionPosteriorState {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive section weights (Thompson or deterministic depending on epoch).
pub fn derive_weights(
    state: &SectionPosteriorState,
    rng: &mut impl Rng,
) -> HashMap<SectionType, f64> {
    state.weights(rng)
}

/// Derive deterministic weights from posterior means.
pub fn derive_deterministic_weights(state: &SectionPosteriorState) -> HashMap<SectionType, f64> {
    state.deterministic_weights()
}

// ── Snapshot ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionPosteriorSnapshot {
    pub posteriors: HashMap<SectionType, BetaPosterior>,
    pub priors: HashMap<SectionType, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

// ── Sampling helpers ────────────────────────────────────────────────────────

fn apply_floor_and_renorm(weights: &mut HashMap<SectionType, f64>, floor: f64) {
    let sum: f64 = weights.values().sum();
    if sum > 0.0 {
        for v in weights.values_mut() {
            *v /= sum;
        }
    }
    for _ in 0..20 {
        let (pinned_sum, n_free) = weights.values().fold((0.0f64, 0usize), |(ps, nf), &w| {
            if w <= floor {
                (ps + floor, nf)
            } else {
                (ps, nf + 1)
            }
        });
        if n_free == 0 {
            let total: f64 = weights.values().sum();
            if total > 0.0 {
                for v in weights.values_mut() {
                    *v /= total;
                }
            }
            break;
        }
        let free_mass = (1.0 - pinned_sum).max(0.0);
        let free_sum: f64 = weights.values().filter(|&&w| w > floor).sum();
        for v in weights.values_mut() {
            if *v <= floor {
                *v = floor;
            } else if free_sum > 0.0 {
                *v = (*v / free_sum) * free_mass;
            }
        }
        if weights.values().all(|&w| w >= floor - 1e-12) {
            break;
        }
    }
}

fn sample_beta_gamma(alpha: f64, beta: f64, rng: &mut impl Rng) -> f64 {
    let x = sample_gamma_mt(alpha, rng);
    let y = sample_gamma_mt(beta, rng);
    let s = x + y;
    if s <= 0.0 {
        0.5
    } else {
        x / s
    }
}

fn sample_gamma_mt(shape: f64, rng: &mut impl Rng) -> f64 {
    if shape < 1.0 {
        let g = sample_gamma_mt(shape + 1.0, rng);
        let u: f64 = rng.gen();
        return g * u.powf(1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x: f64 = sample_standard_normal_bm(rng);
        let v_raw = 1.0 + c * x;
        if v_raw <= 0.0 {
            continue;
        }
        let v = v_raw * v_raw * v_raw;
        let u: f64 = rng.gen();
        if u < 1.0 - 0.0331 * (x * x) * (x * x) {
            return d * v;
        }
        if u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
            return d * v;
        }
    }
}

fn sample_standard_normal_bm(rng: &mut impl Rng) -> f64 {
    let u1: f64 = rng.gen::<f64>().max(f64::EPSILON);
    let u2: f64 = rng.gen();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn epoch_zero_uses_deterministic() {
        let mut state = SectionPosteriorState::new();
        state.exploration_epoch = 0;
        let mut rng = rand::thread_rng();
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
}
