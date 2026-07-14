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
    /// Panics if `alpha` or `beta` is non-finite or non-positive.
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
    ///
    /// Returns an error if the merged parameters would be non-positive (which can happen when
    /// `prior` exceeds the evidence in either posterior).
    pub fn merge(
        &self,
        other: &BetaPosterior,
        prior: &BetaPosterior,
    ) -> Result<BetaPosterior, String> {
        let alpha = self.alpha + other.alpha - prior.alpha;
        let beta = self.beta + other.beta - prior.beta;
        BetaPosterior::try_new(alpha, beta)
    }

    /// Cap ESS by scaling excess evidence back toward the prior.
    ///
    /// Returns `Err` if `cap` is not finite and positive, if `cap` is less than or equal to
    /// `prior.effective_sample_size()`, or if the resulting scaled parameters are non-positive.
    pub fn apply_ess_cap(&mut self, prior: &BetaPosterior, cap: f64) -> Result<(), String> {
        if !cap.is_finite() || cap <= 0.0 {
            return Err(format!(
                "apply_ess_cap: cap must be finite and positive, got {cap}"
            ));
        }
        let ess = self.effective_sample_size();
        if ess > cap {
            let prior_ess = prior.effective_sample_size();
            if cap <= prior_ess {
                return Err(format!(
                    "apply_ess_cap: cap ({cap}) must be > prior ESS ({prior_ess})"
                ));
            }
            let scale = (cap - prior_ess) / (ess - prior_ess);
            let new_alpha = prior.alpha + (self.alpha - prior.alpha) * scale;
            let new_beta = prior.beta + (self.beta - prior.beta) * scale;
            if new_alpha <= 0.0 || new_beta <= 0.0 {
                return Err(format!(
                    "apply_ess_cap: scaled parameters must be positive, got alpha={new_alpha} beta={new_beta}"
                ));
            }
            self.alpha = new_alpha;
            self.beta = new_beta;
        }
        Ok(())
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

    /// Current eviction order, oldest (next to evict) first.
    pub fn order(&self) -> Vec<Uuid> {
        self.order.iter().copied().collect()
    }

    /// Rebuild from a persisted map plus an explicit eviction order.
    ///
    /// `order` lists ids from least- to most-recently-used, as produced by
    /// [`Self::order`]. Ids in `order` that are absent from `map` are dropped;
    /// map entries missing from `order` (legacy snapshots with no order
    /// metadata, or partially-ordered snapshots) are appended afterward in
    /// ascending `Uuid` order so restore is deterministic across processes.
    /// The combined id list is then truncated to `capacity`, so a snapshot
    /// with more entries than the configured cache capacity restores bounded
    /// rather than exceeding it.
    pub fn from_snapshot(
        map: HashMap<Uuid, BetaPosterior>,
        order: Vec<Uuid>,
        capacity: usize,
    ) -> Self {
        let mut seen = std::collections::HashSet::with_capacity(map.len());
        let mut ids: Vec<Uuid> = Vec::with_capacity(map.len());

        for id in order {
            if map.contains_key(&id) && seen.insert(id) {
                ids.push(id);
            }
        }

        let mut remaining: Vec<Uuid> = map
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        remaining.sort();
        ids.extend(remaining);
        ids.truncate(capacity);

        let mut ep = Self::new(capacity);
        for id in ids {
            if let Some(posterior) = map.get(&id).cloned() {
                ep.map.insert(id, posterior);
                ep.order.push_back(id);
            }
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
        let merged = a.merge(&b, &prior).expect("valid merge");
        assert!((merged.alpha - 8.0).abs() < 1e-12);
        assert!((merged.beta - 8.0).abs() < 1e-12);
    }

    #[test]
    fn merge_returns_err_when_prior_exceeds_evidence() {
        // If prior alpha > self.alpha + other.alpha, merged alpha would be non-positive.
        let prior = BetaPosterior::new(10.0, 1.0);
        let a = BetaPosterior::new(3.0, 1.0);
        let b = BetaPosterior::new(3.0, 1.0);
        // merged alpha = 3 + 3 - 10 = -4 → must be rejected
        assert!(
            a.merge(&b, &prior).is_err(),
            "merge yielding non-positive alpha must be rejected"
        );
    }

    #[test]
    fn apply_ess_cap_validates_cap_arg() {
        let prior = BetaPosterior::new(1.0, 1.0);
        let mut p = BetaPosterior::new(60.0, 50.0);
        // Valid cap must work.
        p.apply_ess_cap(&prior, 100.0)
            .expect("valid cap must succeed");
        assert!((p.effective_sample_size() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn apply_ess_cap_returns_err_on_zero_cap() {
        let prior = BetaPosterior::new(1.0, 1.0);
        let mut p = BetaPosterior::new(10.0, 10.0);
        let result = p.apply_ess_cap(&prior, 0.0);
        assert!(result.is_err(), "zero cap must return Err");
        assert!(
            result
                .unwrap_err()
                .contains("cap must be finite and positive"),
            "error message must name the constraint"
        );
    }

    #[test]
    fn apply_ess_cap_returns_err_on_nan_cap() {
        let prior = BetaPosterior::new(1.0, 1.0);
        let mut p = BetaPosterior::new(10.0, 10.0);
        let result = p.apply_ess_cap(&prior, f64::NAN);
        assert!(result.is_err(), "NaN cap must return Err");
    }

    #[test]
    fn apply_ess_cap_returns_err_when_cap_le_prior_ess() {
        // prior ESS = 200.0; cap = 100.0 < prior ESS → must return Err, not panic
        let prior = BetaPosterior::new(100.0, 100.0);
        let mut p = BetaPosterior::new(200.0, 200.0);
        let result = p.apply_ess_cap(&prior, 100.0);
        assert!(result.is_err(), "cap <= prior_ess must return Err");
        assert!(
            result.unwrap_err().contains("must be > prior ESS"),
            "error must state cap vs prior_ess constraint"
        );
    }

    #[test]
    fn ess_cap() {
        let prior = BetaPosterior::new(2.0, 2.0);
        let mut p = BetaPosterior::new(60.0, 50.0);
        p.apply_ess_cap(&prior, 100.0).expect("valid cap");
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

    /// JSON does not encode NaN as a token; serde_json parser rejects any literal NaN.
    #[test]
    fn serde_json_rejects_nan_literal_alpha() {
        // "NaN" is not valid JSON; the parser fails before TryFrom is even reached.
        let json = r#"{"alpha": NaN, "beta": 1.0}"#;
        let result: Result<BetaPosterior, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "JSON literal NaN must be rejected by the parser"
        );
    }

    /// JSON Infinity literal is not valid JSON; the parser rejects it.
    #[test]
    fn serde_json_rejects_infinity_literal_alpha() {
        let json = r#"{"alpha": Infinity, "beta": 1.0}"#;
        let result: Result<BetaPosterior, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "JSON literal Infinity must be rejected by the parser"
        );
    }

    /// Very large exponent (1e400) overflows to f64::INFINITY; TryFrom rejects it.
    #[test]
    fn serde_json_rejects_overflow_to_infinity_alpha() {
        let json = r#"{"alpha": 1e400, "beta": 1.0}"#;
        let result: Result<BetaPosterior, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "1e400 overflowing to infinity must be rejected"
        );
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

    /// BRAINCORE-AUD-001: eviction equivalence across snapshot/restore.
    /// See crates/khive-brain-core/docs/testing-strategy.md#posteriorrssnapshot_restore_eviction_equivalence-braincore-aud-001
    #[test]
    fn snapshot_restore_eviction_equivalence() {
        let capacity = 2;
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        let mut live = EntityPosteriors::new(capacity);
        live.get_or_insert(id_a, BetaPosterior::default);
        live.get_or_insert(id_b, BetaPosterior::default);

        let map = live.to_snapshot();
        let order = live.order();
        let mut restored = EntityPosteriors::from_snapshot(map, order, capacity);

        restored.get_or_insert(id_c, BetaPosterior::default);

        assert_eq!(restored.len(), 2);
        assert!(
            restored.get(&id_a).is_none(),
            "A must be evicted after restore, matching uninterrupted execution"
        );
        assert!(restored.get(&id_b).is_some(), "B must survive eviction");
        assert!(
            restored.get(&id_c).is_some(),
            "C must be present after insert"
        );
    }

    /// BRAINCORE-AUD-001: oversized snapshot restore is bounded by capacity.
    /// See crates/khive-brain-core/docs/testing-strategy.md#posteriorrsoversized_snapshot_restore_is_bounded_by_capacity-braincore-aud-001
    #[test]
    fn oversized_snapshot_restore_is_bounded_by_capacity() {
        let capacity = 2;
        let mut map = HashMap::new();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let id = Uuid::new_v4();
            map.insert(id, BetaPosterior::default());
            ids.push(id);
        }
        // No order metadata supplied — exercises the legacy/deterministic path.
        let restored = EntityPosteriors::from_snapshot(map, Vec::new(), capacity);

        assert_eq!(
            restored.len(),
            capacity,
            "restore must truncate to capacity, not accept all 5 entries"
        );
    }
}
