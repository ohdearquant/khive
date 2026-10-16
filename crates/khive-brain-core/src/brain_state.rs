//! BrainState — profile registry, resolution, and snapshot.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::profile::{
    BalancedRecallSnapshot, BalancedRecallState, ProfileBinding, ProfileLifecycle, ProfileRecord,
};
use crate::section_state::{SectionPosteriorSnapshot, SectionPosteriorState};

/// Sort a slice of profile candidates in ascending `(created_at, id)` order.
///
/// This is the canonical fallback selection key: earliest creation time wins,
/// with alphabetical `id` as a tiebreaker. See
/// crates/khive-brain-core/docs/design-rationale.md#sort_fallback_candidates--why-its-a-standalone-helper
/// for why this is a standalone helper rather than inlined.
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
    /// Dispatch-signal counters for this process. Diagnostics only: they are
    /// not part of the persisted snapshot, they start at zero on every load,
    /// and nothing reads them to make a decision. They exist so the cost of
    /// the signal path has a denominator: a duty cycle with no signal count
    /// beside it cannot say whether the work was too much or merely frequent.
    pub signals_applied: AtomicU64,
    /// Profile-record snapshot serializations performed in this process. This
    /// counts every one, not only the dispatch path's, because the question it
    /// answers is how much serialization the process is doing; the signal
    /// count beside it is what makes the ratio readable. It is atomic because
    /// serialization now happens on the read path, which holds `&self`.
    pub snapshot_serializations: AtomicU64,
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
            signals_applied: AtomicU64::new(0),
            snapshot_serializations: AtomicU64::new(0),
        }
    }

    /// Serialize the current state to a `BrainStateSnapshot` for persistence.
    pub fn to_snapshot(&self) -> BrainStateSnapshot {
        let view = self.snapshot_view();
        BrainStateSnapshot {
            profiles: view.profiles,
            balanced_recall: view.balanced_recall,
            profile_states: view.profile_states,
            bindings: view.bindings.to_vec(),
            section_states: view.section_states,
            router_state: view.router_state.clone(),
            adapter_set: view.adapter_set.clone(),
        }
    }

    /// Encode the same persisted snapshot without copying opaque router bytes
    /// or adapter records into a second owned snapshot. The live posterior
    /// projections are still materialized exactly as in `to_snapshot`.
    pub fn to_snapshot_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.snapshot_view())
    }

    fn snapshot_view(&self) -> BrainStateSnapshotView<'_> {
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
        let profiles = self
            .profiles
            .keys()
            .filter_map(|id| self.materialized_profile(id).map(|r| (id.clone(), r)))
            .collect();
        BrainStateSnapshotView {
            profiles,
            balanced_recall: self.balanced_recall.to_snapshot(),
            profile_states: extra,
            bindings: &self.bindings,
            section_states,
            router_state: &self.router_state,
            adapter_set: &self.adapter_set,
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
            signals_applied: AtomicU64::new(0),
            snapshot_serializations: AtomicU64::new(0),
        }
    }

    /// The live posterior state that owns `profile_id`, if this process holds
    /// one. The default profile lives in its own field for historical reasons;
    /// every other profile lives in the map.
    fn live_profile_state(&self, profile_id: &str) -> Option<&BalancedRecallState> {
        if profile_id == "balanced-recall-v1" {
            Some(&self.balanced_recall)
        } else {
            self.profile_states.get(profile_id)
        }
    }

    /// A profile record whose `state_snapshot` is serialized from live state
    /// at the moment it is asked for.
    ///
    /// This is where the profile snapshot is serialized now. The signal path
    /// used to do it on every signal, which made the cost of a read the cost
    /// of every write that preceded it; the stored value was written far more
    /// often than it was read. A profile the process holds no live state for
    /// (archived, or belonging to another namespace) keeps whatever snapshot
    /// it was loaded with, because there is nothing newer to serialize.
    pub fn materialized_profile(&self, profile_id: &str) -> Option<ProfileRecord> {
        let record = self.profiles.get(profile_id)?;
        let Some(live) = self.live_profile_state(profile_id) else {
            return Some(record.clone());
        };
        let mut record = record.clone();
        record.total_events = live.total_events;
        record.state_snapshot = serde_json::to_value(live.to_snapshot()).ok();
        self.snapshot_serializations.fetch_add(1, Ordering::Relaxed);
        Some(record)
    }

    /// Reset all posteriors to their prior values and bump the exploration epoch.
    pub fn reset_posteriors(&mut self) {
        let mut serialized = false;
        self.balanced_recall.reset_posteriors();
        if let Some(record) = self.profiles.get_mut("balanced-recall-v1") {
            record.exploration_epoch = self.balanced_recall.exploration_epoch;
            record.state_snapshot = serde_json::to_value(self.balanced_recall.to_snapshot()).ok();
            serialized = true;
        }
        if serialized {
            self.snapshot_serializations.fetch_add(1, Ordering::Relaxed);
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
                self.snapshot_serializations.fetch_add(1, Ordering::Relaxed);
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

/// Serialization-only view; field names and values match the owned snapshot.
#[derive(Serialize)]
struct BrainStateSnapshotView<'a> {
    profiles: HashMap<String, ProfileRecord>,
    balanced_recall: BalancedRecallSnapshot,
    profile_states: HashMap<String, BalancedRecallSnapshot>,
    bindings: &'a [ProfileBinding],
    section_states: HashMap<String, SectionPosteriorSnapshot>,
    router_state: &'a HashMap<String, RouterStateBlob>,
    adapter_set: &'a HashMap<String, Vec<AdapterRecord>>,
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
/// ids absent from `entity_posteriors`.
///
/// `entity_posteriors_version` gates how strictly `entity_posterior_order` is
/// checked:
/// - version `0` (legacy, pre-ordering snapshots) — order MUST be empty;
///   restore falls back to the deterministic ascending-`Uuid` compatibility
///   path in [`crate::posterior::EntityPosteriors::from_snapshot`].
/// - version `1` (current) — order MUST cover every `entity_posteriors` key
///   exactly (same length, no duplicates, no unknown ids).
/// - any other version — rejected outright as unknown.
///
/// See crates/khive-brain-core/docs/api/snapshot-validation.md#validate_brain_state_snapshot_with_capacity--entity_posterior_order-version-gating-rationale
/// for why each of these is rejected outright rather than silently repaired.
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

        // Version 0 requires an EMPTY order; a non-empty order here is
        // corruption, not compatibility data — see design-notes.md.
        if br.entity_posteriors_version == 0 && !br.entity_posterior_order.is_empty() {
            return Err(format!(
                "{label}.entity_posterior_order: non-empty order requires entity_posteriors_version >= 1 (got version 0, the legacy empty-order compatibility version)",
            ));
        }

        // Version >= 1 must have full order coverage — see design-notes.md.
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
#[path = "brain_state_tests.rs"]
mod tests;
