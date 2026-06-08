//! Beta-Binomial posterior primitive.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Raw wire format for [`BetaPosterior`]; validated via `TryFrom`.
#[derive(Deserialize)]
struct BetaPosteriorRaw {
    alpha: f64,
    beta: f64,
}

impl TryFrom<BetaPosteriorRaw> for BetaPosterior {
    type Error = String;

    fn try_from(raw: BetaPosteriorRaw) -> Result<Self, Self::Error> {
        BetaPosterior::try_new(raw.alpha, raw.beta)
    }
}

/// Beta-Binomial posterior. Deserialization rejects non-finite or non-positive parameters.
/// Fields are private to prevent construction of invalid states without going through
/// `try_new`. Use the accessor methods `alpha()` and `beta()` to read values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "BetaPosteriorRaw")]
pub struct BetaPosterior {
    alpha: f64,
    beta: f64,
}

impl BetaPosterior {
    /// Construct a `BetaPosterior` with the given parameters.
    ///
    /// # Panics
    /// Panics in debug builds if `alpha` or `beta` is non-finite or non-positive.
    /// Use [`try_new`](Self::try_new) when parameters come from untrusted input.
    pub fn new(alpha: f64, beta: f64) -> Self {
        Self::try_new(alpha, beta).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_new(alpha: f64, beta: f64) -> Result<Self, String> {
        if !alpha.is_finite() || alpha <= 0.0 {
            return Err(format!(
                "BetaPosterior: alpha must be finite and positive, got {alpha}"
            ));
        }
        if !beta.is_finite() || beta <= 0.0 {
            return Err(format!(
                "BetaPosterior: beta must be finite and positive, got {beta}"
            ));
        }
        Ok(Self { alpha, beta })
    }

    /// Return the alpha (success pseudo-count) parameter.
    pub fn alpha(&self) -> f64 {
        self.alpha
    }

    /// Return the beta (failure pseudo-count) parameter.
    pub fn beta(&self) -> f64 {
        self.beta
    }

    pub fn validate(&self) -> Result<(), String> {
        Self::try_new(self.alpha, self.beta).map(|_| ())
    }

    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    pub fn variance(&self) -> f64 {
        let n = self.alpha + self.beta;
        (self.alpha * self.beta) / (n * n * (n + 1.0))
    }

    pub fn effective_sample_size(&self) -> f64 {
        self.alpha + self.beta
    }

    pub fn update_success(&mut self) {
        self.alpha += 1.0;
    }

    pub fn update_failure(&mut self) {
        self.beta += 1.0;
    }

    pub fn update_success_weighted(&mut self, weight: f64) {
        assert!(
            weight.is_finite() && weight > 0.0,
            "update_success_weighted: weight must be finite and positive, got {weight}"
        );
        self.alpha += weight;
    }

    pub fn update_failure_weighted(&mut self, weight: f64) {
        assert!(
            weight.is_finite() && weight > 0.0,
            "update_failure_weighted: weight must be finite and positive, got {weight}"
        );
        self.beta += weight;
    }

    /// Combine evidence from two independent observers sharing the same prior.
    pub fn merge(&self, other: &BetaPosterior, prior: &BetaPosterior) -> BetaPosterior {
        BetaPosterior {
            alpha: self.alpha + other.alpha - prior.alpha,
            beta: self.beta + other.beta - prior.beta,
        }
    }

    /// Cap ESS by scaling excess evidence back toward the prior.
    pub fn apply_ess_cap(&mut self, prior: &BetaPosterior, cap: f64) {
        let ess = self.effective_sample_size();
        if ess > cap {
            let prior_ess = prior.effective_sample_size();
            let scale = (cap - prior_ess) / (ess - prior_ess);
            self.alpha = prior.alpha + (self.alpha - prior.alpha) * scale;
            self.beta = prior.beta + (self.beta - prior.beta) * scale;
        }
    }

    pub fn floored_mean(&self, floor: f64) -> f64 {
        self.mean().max(floor)
    }
}

impl Default for BetaPosterior {
    fn default() -> Self {
        Self::new(1.0, 1.0)
    }
}

// ── EntityPosteriors ────────────────────────────────────────────────────────

/// Bounded LRU map for per-entity posteriors.
pub struct EntityPosteriors {
    map: HashMap<Uuid, BetaPosterior>,
    order: VecDeque<Uuid>,
    capacity: usize,
}

impl EntityPosteriors {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn get_or_insert(
        &mut self,
        id: Uuid,
        default: impl FnOnce() -> BetaPosterior,
    ) -> &mut BetaPosterior {
        if !self.map.contains_key(&id) {
            if self.map.len() >= self.capacity {
                if let Some(evicted) = self.order.pop_front() {
                    self.map.remove(&evicted);
                }
            }
            self.map.insert(id, default());
            self.order.push_back(id);
        }
        self.map.get_mut(&id).unwrap()
    }

    pub fn get(&self, id: &Uuid) -> Option<&BetaPosterior> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    pub fn to_snapshot(&self) -> HashMap<Uuid, BetaPosterior> {
        self.map.clone()
    }

    pub fn from_snapshot(snapshot: HashMap<Uuid, BetaPosterior>, capacity: usize) -> Self {
        let mut ep = Self::new(capacity);
        for (id, posterior) in snapshot {
            ep.map.insert(id, posterior);
            ep.order.push_back(id);
        }
        ep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beta_posterior_mean() {
        let p = BetaPosterior::new(7.0, 3.0);
        assert!((p.mean() - 0.7).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_variance() {
        let p = BetaPosterior::new(7.0, 3.0);
        let expected = 21.0 / 1100.0;
        assert!((p.variance() - expected).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_ess() {
        let p = BetaPosterior::new(7.0, 3.0);
        assert!((p.effective_sample_size() - 10.0).abs() < 1e-12);
    }

    #[test]
    fn merge_formula() {
        let prior = BetaPosterior::new(1.0, 1.0);
        let a = BetaPosterior::new(5.0, 3.0);
        let b = BetaPosterior::new(4.0, 6.0);
        let merged = a.merge(&b, &prior);
        assert!((merged.alpha - 8.0).abs() < 1e-12);
        assert!((merged.beta - 8.0).abs() < 1e-12);
    }

    #[test]
    fn ess_cap() {
        let prior = BetaPosterior::new(2.0, 2.0);
        let mut p = BetaPosterior::new(60.0, 50.0);
        p.apply_ess_cap(&prior, 100.0);
        assert!((p.effective_sample_size() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn try_new_rejects_invalid() {
        assert!(BetaPosterior::try_new(0.0, 1.0).is_err());
        assert!(BetaPosterior::try_new(1.0, -1.0).is_err());
        assert!(BetaPosterior::try_new(f64::NAN, 1.0).is_err());
        assert!(BetaPosterior::try_new(f64::INFINITY, 1.0).is_err());
    }

    #[test]
    fn serde_rejects_null_alpha() {
        // This tests that null (invalid f64) is rejected — NOT a NaN test.
        let json = r#"{"alpha": null, "beta": 1.0}"#;
        let result: Result<BetaPosterior, _> = serde_json::from_str(json);
        assert!(result.is_err(), "null alpha must be rejected");
    }

    /// Verify that NaN is rejected at the serde boundary via `try_from`.
    /// JSON cannot encode NaN natively; this tests the `TryFrom<BetaPosteriorRaw>` path
    /// which is the actual validation gate that `#[serde(try_from)]` invokes.
    #[test]
    fn serde_rejects_nan_alpha_via_try_from() {
        let raw_nan: BetaPosteriorRaw = BetaPosteriorRaw {
            alpha: f64::NAN,
            beta: 1.0,
        };
        let result = BetaPosterior::try_from(raw_nan);
        assert!(result.is_err(), "NaN alpha must be rejected via try_from");
    }

    /// Verify that NaN is rejected at the serde boundary via `try_from` for beta.
    #[test]
    fn serde_rejects_nan_beta_via_try_from() {
        let raw_nan: BetaPosteriorRaw = BetaPosteriorRaw {
            alpha: 1.0,
            beta: f64::NAN,
        };
        let result = BetaPosterior::try_from(raw_nan);
        assert!(result.is_err(), "NaN beta must be rejected via try_from");
    }

    #[test]
    fn serde_rejects_nonfinite_beta() {
        let raw = BetaPosteriorRaw {
            alpha: 1.0,
            beta: f64::INFINITY,
        };
        let result = BetaPosterior::try_from(raw);
        assert!(result.is_err(), "Inf beta must be rejected via try_from");
    }

    #[test]
    fn serde_rejects_negative_alpha() {
        let raw = BetaPosteriorRaw {
            alpha: -1.0,
            beta: 1.0,
        };
        let result = BetaPosterior::try_from(raw);
        assert!(
            result.is_err(),
            "negative alpha must be rejected via try_from"
        );
    }

    #[test]
    fn serde_roundtrip_valid_posterior() {
        let p = BetaPosterior::new(2.5, 3.5);
        let json = serde_json::to_string(&p).unwrap();
        let restored: BetaPosterior = serde_json::from_str(&json).unwrap();
        assert!((restored.alpha - 2.5).abs() < 1e-12);
        assert!((restored.beta - 3.5).abs() < 1e-12);
    }

    #[test]
    fn entity_posteriors_lru_eviction() {
        let mut ep = EntityPosteriors::new(2);
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();
        ep.get_or_insert(id1, BetaPosterior::default);
        ep.get_or_insert(id2, BetaPosterior::default);
        assert_eq!(ep.len(), 2);
        ep.get_or_insert(id3, BetaPosterior::default);
        assert_eq!(ep.len(), 2);
        assert!(ep.get(&id1).is_none());
        assert!(ep.get(&id2).is_some());
        assert!(ep.get(&id3).is_some());
    }
}
