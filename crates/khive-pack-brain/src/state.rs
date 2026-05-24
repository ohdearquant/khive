use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── BetaPosterior ─────────────────────────────────────────────────────────────

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

    /// Combine evidence from two independent observers sharing the same prior.
    ///
    /// merged = Beta(a₁ + a₂ − a_prior, b₁ + b₂ − b_prior)
    pub fn merge(&self, other: &BetaPosterior, prior: &BetaPosterior) -> BetaPosterior {
        BetaPosterior {
            alpha: self.alpha + other.alpha - prior.alpha,
            beta: self.beta + other.beta - prior.beta,
        }
    }
}

impl Default for BetaPosterior {
    fn default() -> Self {
        Self::new(1.0, 1.0)
    }
}

// ── EntityPosteriors ──────────────────────────────────────────────────────────

/// Bounded LRU map for per-entity posteriors.
/// Uses a VecDeque to track insertion order; evicts oldest on insert when full.
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

// ── BalancedRecallState ───────────────────────────────────────────────────────

/// State for the `BalancedRecallProfile` — the v1 default profile.
///
/// Migrated from the predecessor scalar `BrainState` design (ADR-032 §5a).
/// Three-parameter Beta posteriors with informative priors + per-entity LRU.
pub struct BalancedRecallState {
    /// relevance_weight — prior Beta(7,3): warm-starts expecting 70% success
    pub relevance: BetaPosterior,
    /// importance_weight — prior Beta(2,8)
    pub importance: BetaPosterior,
    /// temporal_weight — prior Beta(1,9)
    pub temporal: BetaPosterior,
    /// Per-entity posteriors, bounded LRU (10K default)
    pub entity_posteriors: EntityPosteriors,
    /// Total events processed by this profile
    pub total_events: u64,
    /// Incremented each time posteriors are reset to priors
    pub exploration_epoch: u64,
}

impl BalancedRecallState {
    pub fn new(entity_capacity: usize) -> Self {
        Self {
            relevance: BetaPosterior::new(7.0, 3.0),
            importance: BetaPosterior::new(2.0, 8.0),
            temporal: BetaPosterior::new(1.0, 9.0),
            entity_posteriors: EntityPosteriors::new(entity_capacity),
            total_events: 0,
            exploration_epoch: 0,
        }
    }

    pub fn reset_posteriors(&mut self) {
        self.relevance = BetaPosterior::new(7.0, 3.0);
        self.importance = BetaPosterior::new(2.0, 8.0);
        self.temporal = BetaPosterior::new(1.0, 9.0);
        self.entity_posteriors.clear();
        self.exploration_epoch += 1;
    }

    pub fn to_snapshot(&self) -> BalancedRecallSnapshot {
        BalancedRecallSnapshot {
            relevance: self.relevance.clone(),
            importance: self.importance.clone(),
            temporal: self.temporal.clone(),
            entity_posteriors: self.entity_posteriors.to_snapshot(),
            total_events: self.total_events,
            exploration_epoch: self.exploration_epoch,
        }
    }

    pub fn from_snapshot(snapshot: BalancedRecallSnapshot, entity_capacity: usize) -> Self {
        Self {
            relevance: snapshot.relevance,
            importance: snapshot.importance,
            temporal: snapshot.temporal,
            entity_posteriors: EntityPosteriors::from_snapshot(
                snapshot.entity_posteriors,
                entity_capacity,
            ),
            total_events: snapshot.total_events,
            exploration_epoch: snapshot.exploration_epoch,
        }
    }
}

/// Serializable snapshot of `BalancedRecallState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancedRecallSnapshot {
    pub relevance: BetaPosterior,
    pub importance: BetaPosterior,
    pub temporal: BetaPosterior,
    pub entity_posteriors: HashMap<Uuid, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

// ── ProfileLifecycle ──────────────────────────────────────────────────────────

/// Lifecycle states for a registered profile (ADR-032 §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLifecycle {
    /// Profile code and metadata exist; not yet registered with brain.
    Defined,
    /// Brain knows about it; backtest-eligible. Not yet in live update loop.
    Registered,
    /// Live update loop running; snapshots persist.
    Active,
    /// Registered but no live updates. State retained; read-only.
    Inactive,
    /// Live updates stopped; snapshots and event log retained for audit.
    Archived,
}

// ── ProfileRecord ─────────────────────────────────────────────────────────────

/// Profile metadata stored in the registry (ADR-032 §2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRecord {
    pub id: String,
    pub description: String,
    pub consumer_kind: String,
    pub state_class: String,
    pub lifecycle: ProfileLifecycle,
    pub created_at: DateTime<Utc>,
    /// Serialized state snapshot (opaque bytes to brain core)
    pub state_snapshot: Option<serde_json::Value>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl ProfileRecord {
    pub fn new_balanced_recall(entity_capacity: usize) -> Self {
        let state = BalancedRecallState::new(entity_capacity);
        let snapshot = state.to_snapshot();
        Self {
            id: "balanced-recall-v1".into(),
            description: "Default recall profile: three-scalar Beta posteriors (ADR-032 §5a)"
                .into(),
            consumer_kind: "recall".into(),
            state_class: "Bayesian".into(),
            lifecycle: ProfileLifecycle::Active,
            created_at: Utc::now(),
            state_snapshot: serde_json::to_value(snapshot).ok(),
            total_events: 0,
            exploration_epoch: 0,
        }
    }
}

// ── ProfileBinding ────────────────────────────────────────────────────────────

/// One row in the profile binding table (ADR-032 §10).
///
/// Resolution uses longest-match wins; `*` is the wildcard sentinel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileBinding {
    pub actor: String,
    pub namespace: String,
    pub consumer_kind: String,
    pub profile_id: String,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
}

// ── BrainState (profile registry) ────────────────────────────────────────────

/// Runtime brain state — profile registry + active state per profile.
///
/// ADR-032 §1: BrainState holds profile registry and lifecycle metadata.
/// Posteriors live inside each profile's own state, opaque to brain.
pub struct BrainState {
    /// Registered profiles indexed by profile_id.
    pub profiles: HashMap<String, ProfileRecord>,
    /// In-memory BalancedRecallState for the active default profile.
    pub balanced_recall: BalancedRecallState,
    /// Profile binding table — maps (actor, namespace, consumer_kind) → profile_id.
    pub bindings: Vec<ProfileBinding>,
}

impl BrainState {
    pub fn new(entity_capacity: usize) -> Self {
        let mut profiles = HashMap::new();
        let record = ProfileRecord::new_balanced_recall(entity_capacity);
        profiles.insert(record.id.clone(), record);
        Self {
            profiles,
            balanced_recall: BalancedRecallState::new(entity_capacity),
            bindings: Vec::new(),
        }
    }

    pub fn to_snapshot(&self) -> BrainStateSnapshot {
        BrainStateSnapshot {
            profiles: self.profiles.clone(),
            balanced_recall: self.balanced_recall.to_snapshot(),
            bindings: self.bindings.clone(),
        }
    }

    pub fn from_snapshot(snapshot: BrainStateSnapshot, entity_capacity: usize) -> Self {
        Self {
            profiles: snapshot.profiles,
            balanced_recall: BalancedRecallState::from_snapshot(
                snapshot.balanced_recall,
                entity_capacity,
            ),
            bindings: snapshot.bindings,
        }
    }

    /// Reset the balanced-recall profile posteriors to priors.
    pub fn reset_posteriors(&mut self) {
        self.balanced_recall.reset_posteriors();
        if let Some(record) = self.profiles.get_mut("balanced-recall-v1") {
            record.exploration_epoch = self.balanced_recall.exploration_epoch;
            record.state_snapshot = serde_json::to_value(self.balanced_recall.to_snapshot()).ok();
        }
    }

    /// Resolve a profile_id for the given caller context (ADR-032 §10).
    ///
    /// Longest-match wins: actor + namespace + consumer_kind beats actor + consumer_kind
    /// beats namespace + consumer_kind beats consumer_kind alone. Returns the
    /// `balanced-recall-v1` default when no explicit binding matches.
    pub fn resolve(
        &self,
        actor: Option<&str>,
        namespace: Option<&str>,
        consumer_kind: &str,
    ) -> Option<&ProfileRecord> {
        let actor_val = actor.unwrap_or("*");
        let namespace_val = namespace.unwrap_or("*");

        let best = self
            .bindings
            .iter()
            .filter(|b| {
                (b.actor == "*" || b.actor == actor_val)
                    && (b.namespace == "*" || b.namespace == namespace_val)
                    && (b.consumer_kind == "*" || b.consumer_kind == consumer_kind)
            })
            .max_by_key(|b| {
                let actor_score = if b.actor != "*" { 4 } else { 0 };
                let ns_score = if b.namespace != "*" { 2 } else { 0 };
                let kind_score = if b.consumer_kind != "*" { 1 } else { 0 };
                (
                    actor_score + ns_score + kind_score,
                    b.priority,
                    -(b.created_at.timestamp()),
                )
            });

        if let Some(binding) = best {
            return self.profiles.get(&binding.profile_id);
        }

        // No explicit binding — return the named default profile if it exists and is
        // usable, otherwise fall through to any active profile for the consumer_kind.
        // ADR-032 §10: "balanced-recall-v1" is the v1 system-default for recall.
        if let Some(default) = self.profiles.get("balanced-recall-v1") {
            if default.consumer_kind == consumer_kind
                || consumer_kind == "*"
                || default.consumer_kind == "*"
            {
                return Some(default);
            }
        }

        // Generic fallback: first active profile matching consumer_kind.
        self.profiles
            .values()
            .find(|p| p.consumer_kind == consumer_kind && p.lifecycle == ProfileLifecycle::Active)
    }
}

/// Serializable snapshot of the full brain state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStateSnapshot {
    pub profiles: HashMap<String, ProfileRecord>,
    pub balanced_recall: BalancedRecallSnapshot,
    pub bindings: Vec<ProfileBinding>,
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
    fn beta_posterior_merge() {
        let prior = BetaPosterior::new(2.0, 8.0);
        let a = BetaPosterior::new(5.0, 9.0); // prior + 3 success, 1 failure
        let b = BetaPosterior::new(4.0, 10.0); // prior + 2 success, 2 failure
        let merged = a.merge(&b, &prior);
        // merged = (5+4-2, 9+10-8) = (7, 11)
        assert!((merged.alpha - 7.0).abs() < 1e-12);
        assert!((merged.beta - 11.0).abs() < 1e-12);
    }

    #[test]
    fn entity_posteriors_eviction() {
        let mut ep = EntityPosteriors::new(3);
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            ep.get_or_insert(*id, BetaPosterior::default);
        }
        assert_eq!(ep.len(), 3);
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
    fn balanced_recall_state_snapshot_roundtrip() {
        let mut state = BalancedRecallState::new(100);
        state.relevance.update_success();
        state.total_events = 42;
        let id = Uuid::new_v4();
        state
            .entity_posteriors
            .get_or_insert(id, BetaPosterior::default)
            .update_success();

        let snapshot = state.to_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: BalancedRecallSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_events, 42);
        assert!((back.relevance.alpha - 8.0).abs() < 1e-12);
        assert!(back.entity_posteriors.contains_key(&id));
    }

    #[test]
    fn balanced_recall_state_reset_preserves_epoch_increment() {
        let mut state = BalancedRecallState::new(10);
        state.total_events = 100;
        state.reset_posteriors();
        assert_eq!(state.total_events, 100);
        assert_eq!(state.exploration_epoch, 1);
        assert!((state.relevance.alpha - 7.0).abs() < 1e-12);
        assert!((state.relevance.beta - 3.0).abs() < 1e-12);
    }

    #[test]
    fn brain_state_has_balanced_recall_profile_by_default() {
        let state = BrainState::new(100);
        assert!(state.profiles.contains_key("balanced-recall-v1"));
        let record = &state.profiles["balanced-recall-v1"];
        assert_eq!(record.lifecycle, ProfileLifecycle::Active);
        assert_eq!(record.consumer_kind, "recall");
        assert_eq!(record.state_class, "Bayesian");
    }

    #[test]
    fn brain_state_reset_posteriors_updates_record() {
        let mut state = BrainState::new(10);
        state.balanced_recall.relevance.update_success();
        state.balanced_recall.total_events = 50;
        state.reset_posteriors();
        assert_eq!(state.balanced_recall.exploration_epoch, 1);
        let record = &state.profiles["balanced-recall-v1"];
        assert_eq!(record.exploration_epoch, 1);
    }

    #[test]
    fn brain_state_resolve_falls_back_to_default() {
        let state = BrainState::new(100);
        let resolved = state.resolve(None, None, "recall");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "balanced-recall-v1");
    }

    #[test]
    fn brain_state_resolve_uses_explicit_binding() {
        let mut state = BrainState::new(100);
        // Add a second profile
        let mut alt = ProfileRecord::new_balanced_recall(100);
        alt.id = "alt-profile".into();
        state.profiles.insert("alt-profile".into(), alt);

        // Bind alt-profile for actor "agent-1"
        state.bindings.push(ProfileBinding {
            actor: "agent-1".into(),
            namespace: "*".into(),
            consumer_kind: "recall".into(),
            profile_id: "alt-profile".into(),
            priority: 0,
            created_at: Utc::now(),
        });

        let resolved = state.resolve(Some("agent-1"), None, "recall");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "alt-profile");

        // Different actor falls back to default
        let resolved_other = state.resolve(Some("agent-2"), None, "recall");
        assert_eq!(resolved_other.unwrap().id, "balanced-recall-v1");
    }

    #[test]
    fn entity_posteriors_from_snapshot_rebuilds_map() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let mut snapshot = HashMap::new();
        snapshot.insert(id1, BetaPosterior::new(3.0, 2.0));
        snapshot.insert(id2, BetaPosterior::new(5.0, 1.0));

        let ep = EntityPosteriors::from_snapshot(snapshot, 100);
        assert_eq!(ep.len(), 2);
        let p1 = ep.get(&id1).unwrap();
        assert!((p1.alpha - 3.0).abs() < 1e-12);
        let p2 = ep.get(&id2).unwrap();
        assert!((p2.alpha - 5.0).abs() < 1e-12);
    }

    #[test]
    fn brain_state_snapshot_roundtrip() {
        let mut state = BrainState::new(100);
        state.balanced_recall.relevance.update_success();
        state.balanced_recall.total_events = 55;
        state.balanced_recall.exploration_epoch = 2;
        let id = Uuid::new_v4();
        state
            .balanced_recall
            .entity_posteriors
            .get_or_insert(id, || BetaPosterior::new(4.0, 6.0))
            .update_success();

        let snap1 = state.to_snapshot();
        let restored = BrainState::from_snapshot(snap1, 100);
        let snap2 = restored.to_snapshot();

        assert_eq!(snap2.balanced_recall.total_events, 55);
        assert_eq!(snap2.balanced_recall.exploration_epoch, 2);
        assert!((snap2.balanced_recall.relevance.alpha - 8.0).abs() < 1e-12);
        let ep = snap2.balanced_recall.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha - 5.0).abs() < 1e-12);
        assert!((ep.beta - 6.0).abs() < 1e-12);
    }

    #[test]
    fn profile_lifecycle_serde_roundtrip() {
        let lc = ProfileLifecycle::Active;
        let json = serde_json::to_string(&lc).unwrap();
        let back: ProfileLifecycle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProfileLifecycle::Active);
    }

    #[test]
    fn beta_posterior_default_has_uniform_prior() {
        let p = BetaPosterior::default();
        assert!((p.alpha - 1.0).abs() < 1e-12);
        assert!((p.beta - 1.0).abs() < 1e-12);
        assert!((p.mean() - 0.5).abs() < 1e-12);
    }
}
