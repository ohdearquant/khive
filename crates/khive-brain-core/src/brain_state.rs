//! BrainState — profile registry, resolution, and snapshot.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::profile::{
    BalancedRecallSnapshot, BalancedRecallState, ProfileBinding, ProfileLifecycle, ProfileRecord,
};
use crate::section_state::{SectionPosteriorSnapshot, SectionPosteriorState};

/// Sort a slice of profile candidates in ascending `(created_at, id)` order.
///
/// This is the canonical fallback selection key: earliest creation time wins,
/// with alphabetical `id` as a tiebreaker.  Extracted into a standalone helper
/// so it can be unit-tested with intentionally unsorted input, providing
/// fail-before/pass-after coverage that does not rely on `HashMap` randomisation.
pub fn sort_fallback_candidates(candidates: &mut [&ProfileRecord]) {
    candidates.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
}

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
            .map(|(record, _, _)| record)
    }

    /// Resolve a profile for the given context, returning the matched record,
    /// the matched consumer_kind, and whether the result came from an explicit
    /// binding (`matched_binding = true`) vs. a system-default fallback
    /// (`matched_binding = false`).
    ///
    /// Callers that implement the ADR-035 tier-2 / tier-3 split MUST check
    /// `matched_binding`: only a `true` result constitutes a real tier-2 hit.
    /// When `false`, the caller should fall through to tier-3 (pack-local prior).
    pub fn resolve_with_match(
        &self,
        actor: Option<&str>,
        namespace: Option<&str>,
        consumer_kind: &str,
    ) -> Option<(&ProfileRecord, String, bool)> {
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
                // matched_binding = true: came from an explicit binding row.
                return Some((record, binding.consumer_kind.clone(), true));
            }
        }

        if let Some(default) = self.profiles.get("balanced-recall-v1") {
            if default.lifecycle == ProfileLifecycle::Active
                && (default.consumer_kind == consumer_kind
                    || consumer_kind == "*"
                    || default.consumer_kind == "*")
            {
                // matched_binding = false: system-default fallback, not a binding match.
                return Some((default, default.consumer_kind.clone(), false));
            }
        }

        // Sort by (created_at, id) before selecting so the fallback profile is
        // deterministic across processes regardless of HashMap's randomised seed.
        let mut candidates: Vec<&ProfileRecord> = self
            .profiles
            .values()
            .filter(|p| p.consumer_kind == consumer_kind && p.lifecycle == ProfileLifecycle::Active)
            .collect();
        sort_fallback_candidates(&mut candidates);
        candidates
            .into_iter()
            .next()
            // matched_binding = false: active-profile scan fallback, not a binding match.
            .map(|p| (p, p.consumer_kind.clone(), false))
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

    /// `sort_fallback_candidates` must produce `(created_at ASC, id ASC)` order
    /// on an intentionally REVERSE-sorted input vector.
    ///
    /// This is a fail-before/pass-after unit test for the helper itself: the old
    /// `HashMap.values().find_map()` implementation would have returned the first
    /// element from HashMap iteration order (non-deterministic).  The helper under
    /// test sorts its input in-place; passing a reverse-order slice guarantees the
    /// test would fail against any implementation that skips the sort.
    #[test]
    fn sort_fallback_candidates_produces_created_at_then_id_order() {
        // Three profiles inserted in DESCENDING created_at order (worst case for
        // any unsorted implementation).
        let p1 = make_profile("aardvark", "recall", 1_000); // earliest, lowest id
        let p2 = make_profile("mango", "recall", 2_000);
        let p3 = make_profile("zebra", "recall", 3_000); // latest, highest id

        // Intentionally unsorted: reverse order.
        let mut candidates = vec![&p3, &p2, &p1];
        sort_fallback_candidates(&mut candidates);

        // After sort: p1 (earliest) must be first, p3 (latest) must be last.
        assert_eq!(
            candidates[0].id, "aardvark",
            "earliest profile must come first"
        );
        assert_eq!(candidates[1].id, "mango");
        assert_eq!(candidates[2].id, "zebra", "latest profile must come last");
    }

    /// `sort_fallback_candidates` must break ties by `id` (ascending) when
    /// `created_at` is equal.
    #[test]
    fn sort_fallback_candidates_breaks_ties_by_id() {
        let ts = 5_000i64;
        let p_z = make_profile("z-profile", "recall", ts);
        let p_a = make_profile("a-profile", "recall", ts);
        let p_m = make_profile("m-profile", "recall", ts);

        // Deliberately insert in reverse alphabetical order.
        let mut candidates = vec![&p_z, &p_m, &p_a];
        sort_fallback_candidates(&mut candidates);

        assert_eq!(
            candidates[0].id, "a-profile",
            "lowest id must come first on equal timestamps"
        );
        assert_eq!(candidates[1].id, "m-profile");
        assert_eq!(candidates[2].id, "z-profile");
    }

    /// End-to-end: `BrainState::resolve` must select the earliest-created profile
    /// regardless of HashMap insertion order.  This is a secondary integration guard
    /// that complements the helper unit tests above.
    #[test]
    fn resolve_fallback_is_deterministic() {
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

        let id_a = result_a.map(|p| p.id.clone());
        let id_b = result_b.map(|p| p.id.clone());
        assert_eq!(id_a, Some("alpha".to_owned()));
        assert_eq!(id_a, id_b, "fallback resolution must be deterministic");
    }
}
