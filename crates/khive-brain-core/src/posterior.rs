//! Beta-Binomial posterior primitive.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BetaPosterior {
    pub alpha: f64,
    pub beta: f64,
}

impl BetaPosterior {
    pub fn new(alpha: f64, beta: f64) -> Self {
        Self { alpha, beta }
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
        debug_assert!(
            weight > 0.0,
            "update_success_weighted: weight must be positive, got {weight}"
        );
        self.alpha += weight;
    }

    pub fn update_failure_weighted(&mut self, weight: f64) {
        debug_assert!(
            weight > 0.0,
            "update_failure_weighted: weight must be positive, got {weight}"
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
