//! Section posterior updates for the knowledge pack.

use khive_brain_core::{FeedbackSignal, SectionPosteriorState, SectionType, DEFAULT_ESS_CAP};

/// Update section posteriors based on explicit per-section feedback signals.
pub fn on_section_feedback(
    state: &mut SectionPosteriorState,
    signals: &[(SectionType, FeedbackSignal)],
) {
    state.total_events += 1;
    for (section_type, feedback_signal) in signals {
        if let Some(posterior) = state.posteriors.get_mut(section_type) {
            match feedback_signal {
                FeedbackSignal::Useful => posterior.update_success(),
                FeedbackSignal::NotUseful => posterior.update_failure(),
                FeedbackSignal::Wrong => posterior.update_failure_weighted(2.0),
            }
            if let Some(prior) = state.priors.get(section_type).cloned() {
                if let Err(e) = posterior.apply_ess_cap(&prior, DEFAULT_ESS_CAP) {
                    eprintln!(
                        "[knowledge] apply_ess_cap failed for section {:?}: {e}",
                        section_type
                    );
                }
            }
        }
    }
    if state.exploration_epoch > 0 {
        state.exploration_epoch -= 1;
    }
}
