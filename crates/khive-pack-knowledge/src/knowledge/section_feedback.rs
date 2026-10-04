//! Section posterior updates for the knowledge pack.

use khive_brain_core::{FeedbackSignal, SectionPosteriorState, SectionType};

/// Update section posteriors based on explicit per-section feedback signals.
pub fn on_section_feedback(
    state: &mut SectionPosteriorState,
    signals: &[(SectionType, FeedbackSignal)],
) {
    state.total_events += 1;
    for (section_type, feedback_signal) in signals {
        if let Err(e) = state.apply_section_feedback_entry(section_type, feedback_signal, 1.0) {
            eprintln!(
                "[knowledge] apply_ess_cap failed for section {:?}: {e}",
                section_type
            );
        }
    }
    if state.exploration_epoch > 0 {
        state.exploration_epoch -= 1;
    }
}

#[cfg(test)]
#[path = "section_feedback_tests.rs"]
mod tests;
