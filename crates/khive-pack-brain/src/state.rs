use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Beta-Binomial posterior for a single parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BetaPosterior {
    pub alpha: f64,
    pub beta: f64,
}

impl BetaPosterior {
    pub fn new(alpha: f64, beta: f64) -> Self {
        Self { alpha, beta }
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
}

impl Default for BetaPosterior {
    fn default() -> Self {
        Self::new(1.0, 1.0)
    }
}

/// Bounded LRU map for per-entity posteriors.
/// Uses a VecDeque to track access order; evicts oldest on insert when full.
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

/// Runtime brain state — not directly serializable (contains LRU).
pub struct BrainState {
    pub parameters: HashMap<String, BetaPosterior>,
    pub entity_posteriors: EntityPosteriors,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl BrainState {
    pub fn new(parameters: HashMap<String, BetaPosterior>, entity_capacity: usize) -> Self {
        Self {
            parameters,
            entity_posteriors: EntityPosteriors::new(entity_capacity),
            total_events: 0,
            exploration_epoch: 0,
        }
    }

    pub fn to_snapshot(&self) -> BrainStateSnapshot {
        BrainStateSnapshot {
            parameters: self.parameters.clone(),
            entity_posteriors: self.entity_posteriors.to_snapshot(),
            total_events: self.total_events,
            exploration_epoch: self.exploration_epoch,
        }
    }

    pub fn from_snapshot(snapshot: BrainStateSnapshot, entity_capacity: usize) -> Self {
        Self {
            parameters: snapshot.parameters,
            entity_posteriors: EntityPosteriors::from_snapshot(
                snapshot.entity_posteriors,
                entity_capacity,
            ),
            total_events: snapshot.total_events,
            exploration_epoch: snapshot.exploration_epoch,
        }
    }

    pub fn reset_posteriors(&mut self) {
        for posterior in self.parameters.values_mut() {
            *posterior = BetaPosterior::new(1.0, 1.0);
        }
        self.entity_posteriors.clear();
        self.exploration_epoch += 1;
    }
}

/// Serializable snapshot of BrainState for persistence and inspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStateSnapshot {
    pub parameters: HashMap<String, BetaPosterior>,
    pub entity_posteriors: HashMap<Uuid, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
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
        // var = 7*3 / (10*10*11) = 21/1100 ≈ 0.01909
        let expected = 21.0 / 1100.0;
        assert!((p.variance() - expected).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_ess() {
        let p = BetaPosterior::new(7.0, 3.0);
        assert!((p.effective_sample_size() - 10.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_update() {
        let mut p = BetaPosterior::new(1.0, 1.0);
        p.update_success();
        p.update_success();
        p.update_failure();
        assert!((p.alpha - 3.0).abs() < 1e-12);
        assert!((p.beta - 2.0).abs() < 1e-12);
        assert!((p.mean() - 0.6).abs() < 1e-12);
    }

    #[test]
    fn entity_posteriors_eviction() {
        let mut ep = EntityPosteriors::new(3);
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            ep.get_or_insert(*id, BetaPosterior::default);
        }
        assert_eq!(ep.len(), 3);
        // First two should be evicted
        assert!(ep.get(&ids[0]).is_none());
        assert!(ep.get(&ids[1]).is_none());
        assert!(ep.get(&ids[2]).is_some());
        assert!(ep.get(&ids[3]).is_some());
        assert!(ep.get(&ids[4]).is_some());
    }

    #[test]
    fn entity_posteriors_get_or_insert_existing() {
        let mut ep = EntityPosteriors::new(10);
        let id = Uuid::new_v4();
        ep.get_or_insert(id, BetaPosterior::default)
            .update_success();
        let p = ep.get_or_insert(id, BetaPosterior::default);
        assert!((p.alpha - 2.0).abs() < 1e-12);
    }

    #[test]
    fn brain_state_snapshot_roundtrip() {
        let mut state = BrainState::new(HashMap::new(), 100);
        state.parameters.insert(
            "memory::relevance_weight".into(),
            BetaPosterior::new(7.0, 3.0),
        );
        state.total_events = 42;
        let id = Uuid::new_v4();
        state
            .entity_posteriors
            .get_or_insert(id, BetaPosterior::default)
            .update_success();

        let snapshot = state.to_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: BrainStateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_events, 42);
        assert!(back.parameters.contains_key("memory::relevance_weight"));
        assert!(back.entity_posteriors.contains_key(&id));
    }

    #[test]
    fn reset_posteriors_preserves_event_count() {
        let mut params = HashMap::new();
        params.insert("test".into(), BetaPosterior::new(7.0, 3.0));
        let mut state = BrainState::new(params, 10);
        state.total_events = 100;
        state.reset_posteriors();
        assert_eq!(state.total_events, 100);
        assert_eq!(state.exploration_epoch, 1);
        let p = &state.parameters["test"];
        assert!((p.alpha - 1.0).abs() < 1e-12);
        assert!((p.beta - 1.0).abs() < 1e-12);
    }
}
