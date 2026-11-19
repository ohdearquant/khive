//! Section posterior state — Thompson sampling and deterministic weight derivation.

use std::collections::HashMap;

use rand::{Rng, RngExt};
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
/// Standard-deviation multiplier for `lower_confidence_bound`'s pessimistic
/// estimate. `exploration_epoch` decays to 0 permanently (it only ever counts
/// down), after which `deterministic_weights` is the sole scoring path
/// forever — it must not rank a section on posterior mean alone, since mean
/// is well-defined for a 1-observation posterior with no evidence behind it.
pub const DEFAULT_LCB_Z: f64 = 1.0;

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
            let theta =
                sample_beta_gamma(posterior.alpha().max(1e-6), posterior.beta().max(1e-6), rng);
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
            .map(|(&st, p)| (st, lower_confidence_bound(p) / tau))
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
    /// Feedback events with `section_signals` affect section state regardless
    /// of whether their scalar signal uses the legacy or semantic vocabulary.
    pub fn apply_signal(&mut self, signal: &BrainSignal) {
        match signal {
            BrainSignal::Feedback {
                section_signals: Some(signals),
                ..
            } => self.apply_section_signals(signals),
            BrainSignal::SemanticFeedback {
                section_signals: Some(signals),
                effective_weight,
                ..
            } if *effective_weight > 0.0 => {
                self.apply_section_signals_weighted(signals, *effective_weight);
            }
            _ => {}
        }
    }

    /// Apply section evidence without inventing an entity-level feedback signal.
    pub fn apply_section_signals(&mut self, signals: &HashMap<SectionType, FeedbackSignal>) {
        self.apply_section_signals_weighted(signals, 1.0);
    }

    /// Semantic feedback uses its effective weight for section posteriors;
    /// a clamped zero-weight implicit event cannot move them.
    fn apply_section_signals_weighted(
        &mut self,
        signals: &HashMap<SectionType, FeedbackSignal>,
        weight: f64,
    ) {
        self.total_events += 1;

        for (section_type, feedback_signal) in signals {
            if let Err(e) = self.apply_section_feedback_entry(section_type, feedback_signal, weight)
            {
                eprintln!(
                    "[brain-core] apply_ess_cap failed for section {:?}: {e}",
                    section_type
                );
            }
        }

        if self.exploration_epoch > 0 {
            self.exploration_epoch -= 1;
        }
    }

    /// Update one existing section and cap its evidence against its prior.
    /// An existing section requires a finite, positive weight.
    /// Counters, exploration state, iteration order and error reporting belong to the caller.
    pub fn apply_section_feedback_entry(
        &mut self,
        section_type: &SectionType,
        feedback_signal: &FeedbackSignal,
        weight: f64,
    ) -> Result<(), String> {
        if let Some(posterior) = self.posteriors.get_mut(section_type) {
            match feedback_signal {
                FeedbackSignal::Useful => posterior.update_success_weighted(weight),
                FeedbackSignal::NotUseful => posterior.update_failure_weighted(weight),
                FeedbackSignal::Wrong => posterior.update_failure_weighted(2.0 * weight),
            }
            if let Some(prior) = self.priors.get(section_type).cloned() {
                posterior.apply_ess_cap(&prior, DEFAULT_ESS_CAP)?;
            }
        }
        Ok(())
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
    #[serde(deserialize_with = "deserialize_section_posterior_map")]
    pub posteriors: HashMap<SectionType, BetaPosterior>,
    #[serde(deserialize_with = "deserialize_section_posterior_map")]
    pub priors: HashMap<SectionType, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

/// Load a persisted section posterior map, dropping entries keyed by a retired
/// section type (ADR-048, 2026-10-04 amendment). Snapshots written before the
/// retirement carry those keys; the remaining entries load unchanged. Any other
/// key that is not a current section type is still a load error, so this does
/// not loosen `SectionType` itself.
fn deserialize_section_posterior_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<SectionType, BetaPosterior>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = HashMap::<String, BetaPosterior>::deserialize(deserializer)?;
    let mut kept = HashMap::with_capacity(raw.len());
    for (key, posterior) in raw {
        if SectionType::is_retired_name(&key) {
            continue;
        }
        let section = key
            .parse::<SectionType>()
            .map_err(<D::Error as serde::de::Error>::custom)?;
        kept.insert(section, posterior);
    }
    Ok(kept)
}

/// Count-aware pessimistic estimate of a section's posterior: the mean minus
/// `DEFAULT_LCB_Z` standard deviations, floored at 0.
///
/// The raw posterior mean alone cannot distinguish a section with one lucky
/// observation (small `alpha + beta`, high mean, high variance) from one with
/// hundreds of observations at the same or lower mean (large `alpha + beta`,
/// low variance) — `deterministic_weights` would rank the former higher on
/// mean despite having no evidence behind it. Subtracting a variance-scaled
/// penalty shrinks under-evidenced means toward the floor while barely
/// touching well-evidenced ones, without discarding `BetaPosterior`'s existing
/// deterministic `mean()`/`variance()` accessors or introducing randomness.
fn lower_confidence_bound(p: &BetaPosterior) -> f64 {
    (p.mean() - DEFAULT_LCB_Z * p.variance().sqrt()).max(0.0)
}

// ── Sampling helpers ────────────────────────────────────────────────────────

/// Sum in ascending value order. A `HashMap` iterates in a per-instance order
/// and floating-point addition is not associative, so summing in map order lets
/// two maps holding the same weights normalize to results that differ in the
/// last bit.
fn sum_in_value_order(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<f64> = values.collect();
    values.sort_by(f64::total_cmp);
    values.into_iter().sum()
}

fn apply_floor_and_renorm(weights: &mut HashMap<SectionType, f64>, floor: f64) {
    let sum = sum_in_value_order(weights.values().copied());
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
            let total = sum_in_value_order(weights.values().copied());
            if total > 0.0 {
                for v in weights.values_mut() {
                    *v /= total;
                }
            }
            break;
        }
        let free_mass = (1.0 - pinned_sum).max(0.0);
        let free_sum = sum_in_value_order(weights.values().copied().filter(|&w| w > floor));
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
        let u: f64 = rng.random();
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
        let u: f64 = rng.random();
        if u < 1.0 - 0.0331 * (x * x) * (x * x) {
            return d * v;
        }
        if u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
            return d * v;
        }
    }
}

fn sample_standard_normal_bm(rng: &mut impl Rng) -> f64 {
    let u1: f64 = rng.random::<f64>().max(f64::EPSILON);
    let u2: f64 = rng.random();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

#[cfg(test)]
#[path = "section_state_tests.rs"]
mod tests;
