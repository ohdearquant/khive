//! pack-brain — profile management registry for khive.

pub mod fold;
pub mod handlers;
pub mod persist;
pub mod tunable;

mod event;
mod pack;

pub(crate) use pack::sync_balanced_recall_record;
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
/// consistent contract.
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

#[cfg(test)]
mod tests;
