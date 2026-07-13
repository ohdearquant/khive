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
    pub router_state: HashMap<String, RouterStateBlob>,
    pub adapter_set: HashMap<String, Vec<AdapterRecord>>,
}

impl BrainState {
    /// Create a fresh `BrainState` with a single default `balanced-recall-v1` profile.
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
            router_state: HashMap::new(),
            adapter_set: HashMap::new(),
        }
    }

    /// Serialize the current state to a `BrainStateSnapshot` for persistence.
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
            router_state: self.router_state.clone(),
            adapter_set: self.adapter_set.clone(),
        }
    }

    /// Rebuild live state from a persisted snapshot.
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
            router_state: snapshot.router_state,
            adapter_set: snapshot.adapter_set,
        }
    }

    /// Reset all posteriors to their prior values and bump the exploration epoch.
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

    /// Reset posteriors for a single named profile, leaving other profiles unchanged.
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

    /// Resolve which `ProfileRecord` serves a given (actor, namespace, consumer_kind) triple.
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

/// Opaque envelope for gate-engine router weights. Brain stores and
/// round-trips the bytes without parsing them; the gate engine owns
/// the internal layout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouterStateBlob {
    pub schema_version: u32,
    /// Raw serialized gate weights. Treated as opaque bytes by brain.
    pub gate_bytes: Vec<u8>,
}

/// Brain-native integrity record for a single LoRA adapter slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdapterRecord {
    pub adapter_id: String,
    pub slot: u32,
    /// Content hash of the adapter checkpoint, for integrity verification.
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStateSnapshot {
    pub profiles: HashMap<String, ProfileRecord>,
    pub balanced_recall: BalancedRecallSnapshot,
    #[serde(default)]
    pub profile_states: HashMap<String, BalancedRecallSnapshot>,
    pub bindings: Vec<ProfileBinding>,
    #[serde(default)]
    pub section_states: HashMap<String, SectionPosteriorSnapshot>,
    #[serde(default)]
    pub router_state: HashMap<String, RouterStateBlob>,
    #[serde(default)]
    pub adapter_set: HashMap<String, Vec<AdapterRecord>>,
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

/// Validate posterior numerics (via [`validate_brain_state_snapshot`]) plus
/// entity-posterior cache-capacity bounds for a persisted snapshot.
///
/// Rejects a snapshot whose `balanced_recall` or any `profile_states` entry
/// holds more entity posteriors than `entity_capacity`, and rejects
/// `entity_posterior_order` values that contain duplicate ids or reference
/// ids absent from `entity_posteriors`. This is the capacity-aware
/// counterpart used at the persistence load boundary, so a crafted or legacy
/// oversized snapshot is rejected outright rather than silently truncated.
///
/// `entity_posteriors_version` gates how strictly `entity_posterior_order` is
/// checked:
/// - version `0` (legacy, pre-ordering snapshots) — order MUST be empty;
///   restore falls back to the deterministic ascending-`Uuid` compatibility
///   path in [`crate::posterior::EntityPosteriors::from_snapshot`]. A
///   version-0 snapshot with a non-empty order is not legacy compatibility
///   data — it is partial order metadata that `serde(default)` let through
///   (an omitted or explicit-zero version field) and is rejected here.
/// - version `1` (current) — order MUST cover every `entity_posteriors` key
///   exactly (same length, no duplicates, no unknown ids). A partial order on
///   a current-format snapshot is corruption, not a compatibility case, and is
///   rejected here rather than silently normalized at restore.
/// - any other version — rejected outright as unknown.
pub fn validate_brain_state_snapshot_with_capacity(
    snapshot: &BrainStateSnapshot,
    entity_capacity: usize,
) -> Result<(), String> {
    validate_brain_state_snapshot(snapshot)?;

    fn check_recall(
        label: &str,
        br: &BalancedRecallSnapshot,
        entity_capacity: usize,
    ) -> Result<(), String> {
        if br.entity_posteriors.len() > entity_capacity {
            return Err(format!(
                "{label}.entity_posteriors: {} entries exceeds capacity {entity_capacity}",
                br.entity_posteriors.len()
            ));
        }

        if br.entity_posteriors_version > 1 {
            return Err(format!(
                "{label}.entity_posteriors_version: unknown version {}",
                br.entity_posteriors_version
            ));
        }

        if !br.entity_posterior_order.is_empty() {
            let mut seen =
                std::collections::HashSet::with_capacity(br.entity_posterior_order.len());
            for id in &br.entity_posterior_order {
                if !seen.insert(*id) {
                    return Err(format!("{label}.entity_posterior_order: duplicate id {id}"));
                }
                if !br.entity_posteriors.contains_key(id) {
                    return Err(format!(
                        "{label}.entity_posterior_order: id {id} not present in entity_posteriors"
                    ));
                }
            }
        }

        // Version 0 is the legacy, pre-ordering compatibility case and is
        // only valid with an EMPTY order — restore falls back to the
        // deterministic ascending-`Uuid` order in that case. A version-0
        // snapshot with a non-empty (necessarily partial, since full coverage
        // is only ever written under version 1) order is not legacy
        // compatibility data; it is corruption that `serde(default)` would
        // otherwise let through unnoticed (a snapshot with an omitted or
        // explicit-zero version field), and restore would silently normalize
        // it by appending the missing keys (posterior.rs `from_snapshot`).
        // Reject it outright rather than let it round-trip.
        if br.entity_posteriors_version == 0 && !br.entity_posterior_order.is_empty() {
            return Err(format!(
                "{label}.entity_posterior_order: non-empty order requires entity_posteriors_version >= 1 (got version 0, the legacy empty-order compatibility version)",
            ));
        }

        // Non-legacy (version >= 1) snapshots must have an order entry for
        // every entity_posteriors key. Combined with the duplicate/unknown-id
        // checks above (which already guarantee every order id is a distinct,
        // valid map key), an equal length here proves the id sets are
        // identical — no entity_posteriors key is missing from the order.
        if br.entity_posteriors_version >= 1
            && br.entity_posterior_order.len() != br.entity_posteriors.len()
        {
            return Err(format!(
                "{label}.entity_posterior_order: length {} does not cover all {} entity_posteriors entries (version {} requires full order coverage)",
                br.entity_posterior_order.len(),
                br.entity_posteriors.len(),
                br.entity_posteriors_version,
            ));
        }

        Ok(())
    }

    check_recall(
        "balanced_recall",
        &snapshot.balanced_recall,
        entity_capacity,
    )?;
    for (pid, ps) in &snapshot.profile_states {
        check_recall(&format!("profile_states[{pid}]"), ps, entity_capacity)?;
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
            router_state: HashMap::new(),
            adapter_set: HashMap::new(),
        };
        state_a.profiles.insert(p_early.id.clone(), p_early.clone());
        state_a.profiles.insert(p_later.id.clone(), p_later.clone());

        let mut state_b = BrainState {
            profiles: HashMap::new(),
            balanced_recall: BalancedRecallState::new(8),
            profile_states: HashMap::new(),
            bindings: Vec::new(),
            section_states: HashMap::new(),
            router_state: HashMap::new(),
            adapter_set: HashMap::new(),
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

    /// `router_state` and `adapter_set` survive a `to_snapshot` / `from_snapshot`
    /// round-trip byte-for-byte.
    ///
    /// This is a fail-before/pass-after test: it would fail if either conversion
    /// dropped the new fields.
    #[test]
    fn router_state_and_adapter_set_round_trip() {
        let mut state = BrainState::new(8);

        let blob = RouterStateBlob {
            schema_version: 1,
            gate_bytes: vec![1, 2, 3],
        };
        state
            .router_state
            .insert("profile-x".to_owned(), blob.clone());

        let record = AdapterRecord {
            adapter_id: "lora-42".to_owned(),
            slot: 0,
            content_hash: "abc123".to_owned(),
        };
        state
            .adapter_set
            .insert("profile-x".to_owned(), vec![record.clone()]);

        let snapshot = state.to_snapshot();
        let restored = BrainState::from_snapshot(snapshot, 8);

        assert_eq!(
            restored.router_state.get("profile-x"),
            Some(&blob),
            "router_state must survive round-trip"
        );
        assert_eq!(
            restored.adapter_set.get("profile-x"),
            Some(&vec![record]),
            "adapter_set must survive round-trip"
        );
    }

    /// Deserializing a `BrainStateSnapshot` JSON that omits `router_state` and
    /// `adapter_set` must succeed and yield empty maps (no migration required).
    ///
    /// The test serializes a fresh snapshot, strips both keys from the JSON
    /// object, then re-deserializes — proving that `#[serde(default)]` is wired
    /// correctly.
    #[test]
    fn snapshot_missing_router_and_adapter_fields_defaults_to_empty() {
        let state = BrainState::new(8);
        let snapshot = state.to_snapshot();

        let mut value: serde_json::Value =
            serde_json::to_value(&snapshot).expect("serialize snapshot");

        let obj = value.as_object_mut().expect("snapshot is a JSON object");
        obj.remove("router_state");
        obj.remove("adapter_set");

        let restored: BrainStateSnapshot =
            serde_json::from_value(value).expect("deserialize old-format snapshot");

        assert!(
            restored.router_state.is_empty(),
            "router_state must default to empty map when absent from JSON"
        );
        assert!(
            restored.adapter_set.is_empty(),
            "adapter_set must default to empty map when absent from JSON"
        );
    }

    /// BRAINCORE-AUD-001 regression: oversized-snapshot restore is rejected
    /// (bounded/rejected) at the capacity-aware validation boundary.
    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_entity_posteriors_over_capacity() {
        use uuid::Uuid;

        let capacity = 2;
        let mut state = BrainState::new(capacity);
        for _ in 0..(capacity + 1) {
            state
                .balanced_recall
                .entity_posteriors
                .get_or_insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default);
        }
        // Force an over-capacity snapshot directly, since live inserts already
        // enforce the bound — this simulates a crafted/legacy oversized snapshot.
        let mut snapshot = state.to_snapshot();
        snapshot
            .balanced_recall
            .entity_posteriors
            .insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default());

        let result = validate_brain_state_snapshot_with_capacity(&snapshot, capacity);
        assert!(
            result.is_err(),
            "snapshot with entity_posteriors.len() > capacity must be rejected"
        );
    }

    /// BRAINCORE-AUD-001 regression: capacity violation in a named
    /// `profile_states` entry (not just the default profile) must also be
    /// rejected.
    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_profile_state_over_capacity() {
        use uuid::Uuid;

        let capacity = 1;
        let mut state = BrainState::new(capacity);
        let mut extra = BalancedRecallState::new(capacity);
        extra
            .entity_posteriors
            .get_or_insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default);
        state
            .profile_states
            .insert("extra-profile".to_owned(), extra);

        let mut snapshot = state.to_snapshot();
        snapshot
            .profile_states
            .get_mut("extra-profile")
            .unwrap()
            .entity_posteriors
            .insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default());

        let result = validate_brain_state_snapshot_with_capacity(&snapshot, capacity);
        assert!(
            result.is_err(),
            "profile_states entry with entity_posteriors.len() > capacity must be rejected"
        );
    }

    // ── entity_posterior_order strict coverage regression guard (PR #535) ──────────────

    /// Build a fresh version-1 `BalancedRecallSnapshot` with `n` entity
    /// posteriors and a fully-covering order, for mutation in the tests below.
    fn make_versioned_recall_snapshot(capacity: usize, n: usize) -> BalancedRecallSnapshot {
        let mut state = BalancedRecallState::new(capacity);
        for _ in 0..n {
            state.entity_posteriors.get_or_insert(
                uuid::Uuid::new_v4(),
                crate::posterior::BetaPosterior::default,
            );
        }
        state.to_snapshot()
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_missing_order_ids() {
        let capacity = 4;
        let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
        assert_eq!(snapshot.entity_posteriors_version, 1);
        assert_eq!(snapshot.entity_posterior_order.len(), 2);

        // Drop one id from the order — partial coverage on a current-format
        // (version 1) snapshot must be rejected, not silently normalized.
        snapshot.entity_posterior_order.truncate(1);

        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = snapshot;

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_err(),
            "version-1 snapshot with order shorter than entity_posteriors must be rejected"
        );
        assert!(
            result.unwrap_err().contains("does not cover all"),
            "error must name the missing-coverage reason"
        );
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_duplicate_order_ids() {
        let capacity = 4;
        let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
        let dup = snapshot.entity_posterior_order[0];
        snapshot.entity_posterior_order[1] = dup;

        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = snapshot;

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_err(),
            "duplicate entity_posterior_order ids must be rejected"
        );
        assert!(result.unwrap_err().contains("duplicate id"));
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_unknown_order_ids() {
        let capacity = 4;
        let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
        snapshot.entity_posterior_order.push(uuid::Uuid::new_v4());

        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = snapshot;

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_err(),
            "entity_posterior_order id absent from entity_posteriors must be rejected"
        );
        assert!(result
            .unwrap_err()
            .contains("not present in entity_posteriors"));
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_unknown_version() {
        let capacity = 4;
        let mut snapshot = make_versioned_recall_snapshot(capacity, 1);
        snapshot.entity_posteriors_version = 2;

        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = snapshot;

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_err(),
            "unknown entity_posteriors_version must be rejected"
        );
        assert!(result.unwrap_err().contains("unknown version"));
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_accepts_legacy_empty_order() {
        let capacity = 4;
        let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
        // Explicitly-legacy version 0 with no order metadata must still pass —
        // restore falls back to the deterministic ascending-Uuid path.
        snapshot.entity_posteriors_version = 0;
        snapshot.entity_posterior_order.clear();

        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = snapshot;

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_ok(),
            "legacy version-0 snapshot with empty order must be accepted: {result:?}"
        );
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_rejects_version_zero_with_partial_order() {
        // PR #535, finding 2: a version-0 snapshot with a
        // non-empty (here: partial) order is NOT the legacy empty-order
        // compatibility case. `serde(default)` lets `entity_posteriors_version`
        // silently resolve to 0 on an omitted field, so this must be rejected
        // outright rather than passed through to `restore`, which would
        // silently normalize `[A]` order over `{A, B}` posteriors to `[A, B]`.
        let capacity = 4;
        let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
        snapshot.entity_posteriors_version = 0;
        snapshot.entity_posterior_order.truncate(1);

        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = snapshot;

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_err(),
            "version-0 snapshot with non-empty partial order must be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .contains("requires entity_posteriors_version >= 1"),
            "error must name the version-0-with-non-empty-order reason"
        );
    }

    #[test]
    fn validate_brain_state_snapshot_with_capacity_accepts_full_order_current_version() {
        let capacity = 4;
        let mut full_snapshot = BrainState::new(capacity).to_snapshot();
        full_snapshot.balanced_recall = make_versioned_recall_snapshot(capacity, 3);

        let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
        assert!(
            result.is_ok(),
            "version-1 snapshot with a fully-covering order must be accepted: {result:?}"
        );
    }

    /// A validated snapshot must restore→re-snapshot→restore to the same
    /// state — the validation gate must not itself introduce drift, and a
    /// snapshot that passes validation once must keep passing after a
    /// round-trip through `BrainState`.
    #[test]
    fn restore_to_snapshot_restore_idempotency() {
        let capacity = 4;
        let mut state = BrainState::new(capacity);
        for _ in 0..3 {
            state.balanced_recall.entity_posteriors.get_or_insert(
                uuid::Uuid::new_v4(),
                crate::posterior::BetaPosterior::default,
            );
        }

        let snapshot_1 = state.to_snapshot();
        validate_brain_state_snapshot_with_capacity(&snapshot_1, capacity)
            .expect("first snapshot must validate");

        let restored_1 = BrainState::from_snapshot(snapshot_1, capacity);
        let snapshot_2 = restored_1.to_snapshot();
        validate_brain_state_snapshot_with_capacity(&snapshot_2, capacity)
            .expect("round-tripped snapshot must still validate");

        let restored_2 = BrainState::from_snapshot(snapshot_2.clone(), capacity);
        let snapshot_3 = restored_2.to_snapshot();

        assert_eq!(
            snapshot_2.balanced_recall.entity_posteriors,
            snapshot_3.balanced_recall.entity_posteriors,
            "entity_posteriors must be stable across a second restore/snapshot cycle"
        );
        assert_eq!(
            snapshot_2.balanced_recall.entity_posterior_order,
            snapshot_3.balanced_recall.entity_posterior_order,
            "entity_posterior_order must be stable across a second restore/snapshot cycle"
        );
    }
}
