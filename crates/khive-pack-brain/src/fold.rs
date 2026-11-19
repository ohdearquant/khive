//! Beta-posterior fold implementations for brain profiles.

use khive_fold::{fold_fn, FilterFold, Fold, FoldContext, MapFold};
use khive_storage::event::Event;
use khive_types::EventKind;

use crate::event::interpret;
use khive_brain_core::{BalancedRecallState, BrainSignal, SectionPosteriorState};

fn balanced_event_fold(entity_capacity: usize) -> impl Fold<Event, BalancedRecallState> {
    let signals = fold_fn(
        move |_| BalancedRecallState::new(entity_capacity),
        |mut state: BalancedRecallState, signal: &BrainSignal, _| {
            state.apply_signal(signal);
            state
        },
    );
    // Unlike other irrelevant events, unjudged telemetry is not a training event.
    FilterFold::new(MapFold::new(signals, interpret), |event: &Event| {
        event.kind != EventKind::FeedbackUnjudged
    })
}

fn section_event_fold() -> impl Fold<Event, SectionPosteriorState> {
    MapFold::new(
        fold_fn(
            |_| SectionPosteriorState::new(),
            |mut state: SectionPosteriorState, signal: &BrainSignal, _| {
                state.apply_signal(signal);
                state
            },
        ),
        interpret,
    )
}

/// Fold for the `balanced-recall-v1` three-scalar Beta-posterior state.
pub struct BalancedRecallFold {
    entity_capacity: usize,
}

impl BalancedRecallFold {
    /// Create a fold with the given entity capacity for the `balanced-recall-v1` state.
    pub fn new(entity_capacity: usize) -> Self {
        Self { entity_capacity }
    }
}

impl Fold<Event, BalancedRecallState> for BalancedRecallFold {
    fn init(&self, _context: &FoldContext) -> BalancedRecallState {
        BalancedRecallState::new(self.entity_capacity)
    }

    fn reduce(
        &self,
        state: BalancedRecallState,
        event: &Event,
        ctx: &FoldContext,
    ) -> BalancedRecallState {
        balanced_event_fold(self.entity_capacity).reduce(state, event, ctx)
    }

    fn finalize(&self, state: BalancedRecallState, _context: &FoldContext) -> BalancedRecallState {
        state
    }
}

/// Fold for per-profile section posteriors.
pub struct SectionPosteriorFold;

impl SectionPosteriorFold {
    /// Create a default `SectionPosteriorFold`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SectionPosteriorFold {
    fn default() -> Self {
        Self::new()
    }
}

impl Fold<Event, SectionPosteriorState> for SectionPosteriorFold {
    fn init(&self, _context: &FoldContext) -> SectionPosteriorState {
        SectionPosteriorState::new()
    }

    fn reduce(
        &self,
        mut state: SectionPosteriorState,
        event: &Event,
        ctx: &FoldContext,
    ) -> SectionPosteriorState {
        if event.verb == "brain.section_feedback" {
            if let Ok((_, signals)) = crate::section_feedback::decode_event(event) {
                if !signals.is_empty() {
                    state.apply_section_signals(&signals);
                }
            }
            return state;
        }
        section_event_fold().reduce(state, event, ctx)
    }

    fn finalize(
        &self,
        state: SectionPosteriorState,
        _context: &FoldContext,
    ) -> SectionPosteriorState {
        state
    }
}

#[cfg(test)]
mod composition_tests;

#[cfg(test)]
#[path = "fold/tests.rs"]
mod tests;
