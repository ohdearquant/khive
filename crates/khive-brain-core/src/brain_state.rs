//! BrainState — profile registry, resolution, and snapshot.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::profile::{
    BalancedRecallSnapshot, BalancedRecallState, ProfileBinding, ProfileLifecycle, ProfileRecord,
};
use crate::section_state::{SectionPosteriorSnapshot, SectionPosteriorState};

/// Runtime brain state — profile registry + active state per profile.
pub struct BrainState {
    pub profiles: HashMap<String, ProfileRecord>,
    pub balanced_recall: BalancedRecallState,
    pub profile_states: HashMap<String, BalancedRecallState>,
    pub bindings: Vec<ProfileBinding>,
    pub section_states: HashMap<String, SectionPosteriorState>,
}

impl BrainState {
    pub fn new(entity_capacity: usize) -> Self {
        let mut profiles = HashMap::new();
        let record = ProfileRecord::new_balanced_recall(entity_capacity);
        let profile_id = record.id.clone();
        profiles.insert(profile_id, record);

        Self {
            profiles,
            balanced_recall: BalancedRecallState::new(entity_capacity),
            profile_states: HashMap::new(),
            bindings: Vec::new(),
            section_states: HashMap::new(),
        }
    }

    pub fn to_snapshot(&self) -> BrainStateSnapshot {
        let extra: HashMap<String, BalancedRecallSnapshot> = self
            .profile_states
            .iter()
            .map(|(id, s)| (id.clone(), s.to_snapshot()))
            .collect();
        let section_states: HashMap<String, SectionPosteriorSnapshot> = self
            .section_states
            .iter()
            .map(|(id, s)| (id.clone(), s.to_snapshot()))
            .collect();
        BrainStateSnapshot {
            profiles: self.profiles.clone(),
            balanced_recall: self.balanced_recall.to_snapshot(),
            profile_states: extra,
            bindings: self.bindings.clone(),
            section_states,
        }
    }

    pub fn from_snapshot(snapshot: BrainStateSnapshot, entity_capacity: usize) -> Self {
        let extra: HashMap<String, BalancedRecallState> = snapshot
            .profile_states
            .into_iter()
            .map(|(id, s)| (id, BalancedRecallState::from_snapshot(s, entity_capacity)))
            .collect();
        let section_states: HashMap<String, SectionPosteriorState> = snapshot
            .section_states
            .into_iter()
            .map(|(id, s)| (id, SectionPosteriorState::from_snapshot(s)))
            .collect();
        Self {
            profiles: snapshot.profiles,
            balanced_recall: BalancedRecallState::from_snapshot(
                snapshot.balanced_recall,
                entity_capacity,
            ),
            profile_states: extra,
            bindings: snapshot.bindings,
            section_states,
        }
    }

    pub fn reset_posteriors(&mut self) {
        self.balanced_recall.reset_posteriors();
        if let Some(record) = self.profiles.get_mut("balanced-recall-v1") {
            record.exploration_epoch = self.balanced_recall.exploration_epoch;
            record.state_snapshot = serde_json::to_value(self.balanced_recall.to_snapshot()).ok();
        }
        if let Some(ss) = self.section_states.get_mut("balanced-recall-v1") {
            ss.reset_posteriors();
        }
    }

    pub fn reset_profile_posteriors(&mut self, profile_id: &str) {
        if let Some(ps) = self.profile_states.get_mut(profile_id) {
            ps.reset_posteriors();
            let snap = serde_json::to_value(ps.to_snapshot()).ok();
            let epoch = ps.exploration_epoch;
            if let Some(record) = self.profiles.get_mut(profile_id) {
                record.exploration_epoch = epoch;
                record.state_snapshot = snap;
            }
        }
        if let Some(ss) = self.section_states.get_mut(profile_id) {
            ss.reset_posteriors();
        }
    }

    pub fn resolve(
        &self,
        actor: Option<&str>,
        namespace: Option<&str>,
        consumer_kind: &str,
    ) -> Option<&ProfileRecord> {
        self.resolve_with_match(actor, namespace, consumer_kind)
            .map(|(record, _)| record)
    }

    pub fn resolve_with_match(
        &self,
        actor: Option<&str>,
        namespace: Option<&str>,
        consumer_kind: &str,
    ) -> Option<(&ProfileRecord, String)> {
        let actor_val = actor.unwrap_or("*");
        let namespace_val = namespace.unwrap_or("*");

        let best = self
            .bindings
            .iter()
            .filter(|b| {
                (b.actor == "*" || b.actor == actor_val)
                    && (b.namespace == "*" || b.namespace == namespace_val)
                    && (b.consumer_kind == "*" || b.consumer_kind == consumer_kind)
                    && self
                        .profiles
                        .get(&b.profile_id)
                        .is_some_and(|p| p.lifecycle != ProfileLifecycle::Archived)
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
            if let Some(record) = self.profiles.get(&binding.profile_id) {
                return Some((record, binding.consumer_kind.clone()));
            }
        }

        if let Some(default) = self.profiles.get("balanced-recall-v1") {
            if default.lifecycle == ProfileLifecycle::Active
                && (default.consumer_kind == consumer_kind
                    || consumer_kind == "*"
                    || default.consumer_kind == "*")
            {
                return Some((default, default.consumer_kind.clone()));
            }
        }

        // Sort by (created_at, id) before selecting so the fallback profile is
        // deterministic across processes regardless of HashMap's randomised seed.
        let mut candidates: Vec<&ProfileRecord> = self
            .profiles
            .values()
            .filter(|p| p.consumer_kind == consumer_kind && p.lifecycle == ProfileLifecycle::Active)
            .collect();
        candidates.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        candidates
            .into_iter()
            .next()
            .map(|p| (p, p.consumer_kind.clone()))
    }
}

// ── Snapshot ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStateSnapshot {
    pub profiles: HashMap<String, ProfileRecord>,
    pub balanced_recall: BalancedRecallSnapshot,
    #[serde(default)]
    pub profile_states: HashMap<String, BalancedRecallSnapshot>,
    pub bindings: Vec<ProfileBinding>,
    #[serde(default)]
    pub section_states: HashMap<String, SectionPosteriorSnapshot>,
}

/// Validate all BetaPosterior values in a snapshot.
pub fn validate_brain_state_snapshot(snapshot: &BrainStateSnapshot) -> Result<(), String> {
    let br = &snapshot.balanced_recall;
    br.relevance
        .validate()
        .map_err(|e| format!("balanced_recall.relevance: {e}"))?;
    br.salience
        .validate()
        .map_err(|e| format!("balanced_recall.salience: {e}"))?;
    br.temporal
        .validate()
        .map_err(|e| format!("balanced_recall.temporal: {e}"))?;
    for (id, p) in &br.entity_posteriors {
        p.validate()
            .map_err(|e| format!("balanced_recall.entity_posteriors[{id}]: {e}"))?;
    }

    for (pid, ps) in &snapshot.profile_states {
        ps.relevance
            .validate()
            .map_err(|e| format!("profile_states[{pid}].relevance: {e}"))?;
        ps.salience
            .validate()
            .map_err(|e| format!("profile_states[{pid}].salience: {e}"))?;
        ps.temporal
            .validate()
            .map_err(|e| format!("profile_states[{pid}].temporal: {e}"))?;
        for (id, p) in &ps.entity_posteriors {
            p.validate()
                .map_err(|e| format!("profile_states[{pid}].entity_posteriors[{id}]: {e}"))?;
        }
    }

    for (pid, ss) in &snapshot.section_states {
        for (st, p) in &ss.posteriors {
            p.validate()
                .map_err(|e| format!("section_states[{pid}].posteriors[{st:?}]: {e}"))?;
        }
        for (st, p) in &ss.priors {
            p.validate()
                .map_err(|e| format!("section_states[{pid}].priors[{st:?}]: {e}"))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::profile::ProfileLifecycle;

    fn make_profile(id: &str, consumer_kind: &str, ts_secs: i64) -> ProfileRecord {
        ProfileRecord {
            id: id.to_owned(),
            description: String::new(),
            consumer_kind: consumer_kind.to_owned(),
            state_class: "Bayesian".into(),
            lifecycle: ProfileLifecycle::Active,
            created_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            state_snapshot: None,
            total_events: 0,
            exploration_epoch: 0,
        }
    }

    /// Same state must always select the same fallback profile regardless of
    /// the order in which profiles were inserted into the HashMap.
    #[test]
    fn resolve_fallback_is_deterministic() {
        // Build two BrainState instances with the same profiles inserted in
        // different orders.  Both must resolve to the same (earliest created_at,
        // then lowest id) profile.
        let p_early = make_profile("alpha", "recall", 1_000);
        let p_later = make_profile("zeta", "recall", 2_000);

        let mut state_a = BrainState {
            profiles: HashMap::new(),
            balanced_recall: BalancedRecallState::new(8),
            profile_states: HashMap::new(),
            bindings: Vec::new(),
            section_states: HashMap::new(),
        };
        state_a.profiles.insert(p_early.id.clone(), p_early.clone());
        state_a.profiles.insert(p_later.id.clone(), p_later.clone());

        let mut state_b = BrainState {
            profiles: HashMap::new(),
            balanced_recall: BalancedRecallState::new(8),
            profile_states: HashMap::new(),
            bindings: Vec::new(),
            section_states: HashMap::new(),
        };
        // Insert in the opposite order.
        state_b.profiles.insert(p_later.id.clone(), p_later.clone());
        state_b.profiles.insert(p_early.id.clone(), p_early.clone());

        let result_a = state_a.resolve(None, None, "recall");
        let result_b = state_b.resolve(None, None, "recall");

        // Both must resolve to the earliest-created profile ("alpha").
        let id_a = result_a.map(|p| p.id.clone());
        let id_b = result_b.map(|p| p.id.clone());
        assert_eq!(id_a, Some("alpha".to_owned()));
        assert_eq!(id_a, id_b, "fallback resolution must be deterministic");
    }
}
