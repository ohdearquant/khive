//! pack-brain — profile management registry for khive.

pub mod fold;
pub mod fold_gate;
pub mod handlers;
pub mod persist;
pub mod serve_ledger;
pub mod tunable;

mod event;
mod event_counts_grouping;
mod pack;
mod section_feedback;
mod sql;

pub(crate) use pack::{apply_dispatch_signal, sync_balanced_recall_record};
pub use pack::{BrainPack, ENTITY_CACHE_CAPACITY};

use std::collections::HashMap;

use khive_brain_core::{SectionPosteriorState, SectionType};

/// Ensure `profile_id` has a fully-seeded `SectionPosteriorState` in `section_states`.
///
/// Gets-or-inserts the profile entry via `entry(..).or_default()`, then backfills any
/// missing `posteriors`/`priors` slots from `SectionPosteriorState::default_priors()`
/// across all `SectionType::all()` variants.
///
/// Used by both the live `brain.feedback` handler and the persisted event-replay path
/// so that section signals are applied identically regardless of whether the loaded
/// snapshot predates section_states or a new `SectionType` variant was added after it.
pub(crate) fn ensure_section_state_seeded<'a>(
    section_states: &'a mut HashMap<String, SectionPosteriorState>,
    profile_id: &str,
) -> &'a mut SectionPosteriorState {
    let section_state = section_states.entry(profile_id.to_owned()).or_default();
    let defaults = SectionPosteriorState::default_priors();
    for st in SectionType::all() {
        if let Some(prior) = defaults.get(st) {
            section_state
                .posteriors
                .entry(*st)
                .or_insert_with(|| prior.clone());
            section_state
                .priors
                .entry(*st)
                .or_insert_with(|| prior.clone());
        }
    }
    section_state
}

/// Validate a `section_signals` JSON value before any state mutation.
///
/// Enforces the section fold contract (ADR-048): keys must be known `SectionType`
/// names; values must be `useful`, `not_useful`, or `wrong`; and the map must not
/// be empty (an empty map carries no evidence and must not advance posterior state).
///
/// Used by both `brain.feedback` live handler and replay to ensure a single,
/// consistent contract. A retired section type (`SectionType::RETIRED_NAMES`) is not
/// in `SectionType::NAMES`, so a live write naming one is refused as unknown; replay
/// reaches this validator only after `replay_section_signals` has dropped such keys.
pub(crate) fn validate_section_signals(
    ss: &serde_json::Value,
) -> Result<(), khive_runtime::RuntimeError> {
    let obj = ss.as_object().ok_or_else(|| {
        khive_runtime::RuntimeError::InvalidInput(
            "section_signals must be a JSON object mapping section names to signal strings".into(),
        )
    })?;
    if obj.is_empty() {
        return Err(khive_runtime::RuntimeError::InvalidInput(
            "section_signals must not be empty; omit the field entirely to submit feedback \
             without section evidence"
                .into(),
        ));
    }
    // Section fold (ADR-048) only handles useful | not_useful | wrong.
    // Semantic event kinds (explicit_positive, correction, …) belong to the profile-level
    // signal and are not valid per-section values.
    let valid_signals = ["useful", "not_useful", "wrong"];
    let valid_sections = khive_brain_core::SectionType::NAMES;
    for (key, val) in obj {
        if !valid_sections.contains(&key.as_str()) {
            return Err(khive_runtime::RuntimeError::InvalidInput(format!(
                "section_signals: unknown section {key:?}; valid: {}",
                valid_sections.join(", ")
            )));
        }
        let sig = val.as_str().ok_or_else(|| {
            khive_runtime::RuntimeError::InvalidInput(format!(
                "section_signals: signal for section {key:?} must be one of: useful | not_useful | wrong"
            ))
        })?;
        if !valid_signals.contains(&sig) {
            return Err(khive_runtime::RuntimeError::InvalidInput(format!(
                "section_signals: invalid signal {sig:?} for section {key:?}; \
                 valid: {}",
                valid_signals.join(" | ")
            )));
        }
    }
    Ok(())
}

/// Replay-only view of a recorded `section_signals` map: entries keyed by a retired
/// section type are dropped so the remaining signals apply and the event is not
/// quarantined for carrying them (ADR-048, 2026-10-04 amendment). Every other key,
/// and every value, is left for `validate_section_signals` to judge, so any other
/// invalid entry keeps its quarantine behaviour.
///
/// Returns `None` when the map held only retired entries: no section evidence is
/// left, and the event's other signals still apply. A value that is not an object,
/// or an object that was empty to begin with, is returned unchanged for the
/// validator to refuse.
///
/// Live writes never route through this function; they call
/// `validate_section_signals` directly and refuse a retired name as unknown.
pub(crate) fn replay_section_signals(ss: &serde_json::Value) -> Option<serde_json::Value> {
    let Some(obj) = ss.as_object() else {
        return Some(ss.clone());
    };
    let kept: serde_json::Map<String, serde_json::Value> = obj
        .iter()
        .filter(|(key, _)| !SectionType::is_retired_name(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if kept.is_empty() && !obj.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(kept))
    }
}

/// Replay-time check of a recorded `section_signals` value: the live contract of
/// `validate_section_signals`, applied after `replay_section_signals` dropped any
/// retired section keys. A map that held only retired entries has nothing left to
/// refuse.
pub(crate) fn validate_replayed_section_signals(
    ss: &serde_json::Value,
) -> Result<(), khive_runtime::RuntimeError> {
    match replay_section_signals(ss) {
        Some(kept) => validate_section_signals(&kept),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod retired_section_tests;

#[cfg(test)]
mod event_counts_group_tests;

#[cfg(test)]
mod unknown_event_usage_tests;
