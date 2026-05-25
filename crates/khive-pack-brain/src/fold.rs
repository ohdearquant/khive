use khive_fold::{Fold, FoldContext};
use khive_storage::event::Event;

use crate::event::{entity_signal, interpret, is_recall_positive};
use crate::state::{BalancedRecallState, BetaPosterior};

/// Fold for the `BalancedRecallProfile` state (ADR-032 §5a).
///
/// The predecessor design had this fold update a flat `HashMap<String, BetaPosterior>`
/// on the brain's core `BrainState`. Per ADR-032, the three-scalar Bayesian state
/// now lives entirely inside `BalancedRecallProfile` — brain's `BrainState` holds
/// profile registry metadata; posteriors are opaque to brain.
///
/// Deterministic: same events in same order → same `BalancedRecallState`.
pub struct BalancedRecallFold {
    entity_capacity: usize,
}

impl BalancedRecallFold {
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
        mut state: BalancedRecallState,
        event: &Event,
        _ctx: &FoldContext,
    ) -> BalancedRecallState {
        let signal = interpret(event);

        state.total_events += 1;

        // Global recall-relevance parameter update
        if let Some(positive) = is_recall_positive(&signal) {
            if positive {
                state.relevance.update_success();
            } else {
                state.relevance.update_failure();
            }
        }

        // Per-entity posterior updates
        if let Some((entity_id, positive)) = entity_signal(&signal) {
            let posterior = state
                .entity_posteriors
                .get_or_insert(entity_id, || BetaPosterior::new(1.0, 1.0));
            if positive {
                posterior.update_success();
            } else {
                posterior.update_failure();
            }
        }

        state
    }

    fn finalize(&self, state: BalancedRecallState, _context: &FoldContext) -> BalancedRecallState {
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::{EventKind, EventOutcome, SubstrateKind};
    use uuid::Uuid;

    fn make_event(verb: &str, outcome: EventOutcome, target: Option<Uuid>) -> Event {
        let mut e = Event::new("test", verb, EventKind::Audit, SubstrateKind::Note, "brain");
        e.outcome = outcome;
        e.target_id = target;
        e
    }

    #[test]
    fn initial_state_has_informative_priors() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);
        // relevance prior Beta(7,3)
        assert!((state.relevance.alpha - 7.0).abs() < 1e-12);
        assert!((state.relevance.beta - 3.0).abs() < 1e-12);
        // importance prior Beta(2,8)
        assert!((state.importance.alpha - 2.0).abs() < 1e-12);
        assert!((state.importance.beta - 8.0).abs() < 1e-12);
        // temporal prior Beta(1,9)
        assert!((state.temporal.alpha - 1.0).abs() < 1e-12);
        assert!((state.temporal.beta - 9.0).abs() < 1e-12);
    }

    #[test]
    fn recall_hit_updates_relevance_and_entity() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let id = Uuid::new_v4();
        let event = make_event("recall", EventOutcome::Success, Some(id));
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        assert!((state.relevance.alpha - 8.0).abs() < 1e-12); // 7 + 1
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha - 2.0).abs() < 1e-12); // 1 + 1
    }

    #[test]
    fn recall_miss_updates_relevance_beta() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let event = make_event("recall", EventOutcome::Success, None);
        state = fold.reduce(state, &event, &ctx);

        // target_id = None → RecallMiss → relevance failure
        assert!((state.relevance.beta - 4.0).abs() < 1e-12); // 3 + 1
        assert!(state.entity_posteriors.is_empty());
    }

    #[test]
    fn irrelevant_event_increments_counter_only() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let event = make_event("link", EventOutcome::Success, Some(Uuid::new_v4()));
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        assert!((state.relevance.alpha - 7.0).abs() < 1e-12); // unchanged
    }

    #[test]
    fn feedback_not_useful_increments_entity_beta() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let id = Uuid::new_v4();
        let mut event = make_event("brain.feedback", EventOutcome::Success, Some(id));
        event.payload = serde_json::json!({"signal": "not_useful"});
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha - 1.0).abs() < 1e-12);
        assert!((ep.beta - 2.0).abs() < 1e-12);
    }

    #[test]
    fn brain_emit_legacy_does_not_update_entity() {
        // brain.emit is now Irrelevant (ADR-032 migration boundary)
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let id = Uuid::new_v4();
        let mut event = make_event("brain.emit", EventOutcome::Success, Some(id));
        event.payload = serde_json::json!({"signal": "useful"});
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        assert!(state.entity_posteriors.is_empty()); // no entity update from legacy verb
    }

    #[test]
    fn deterministic_replay() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();

        let id = Uuid::new_v4();
        let events = vec![
            make_event("recall", EventOutcome::Success, Some(id)),
            make_event("recall", EventOutcome::Success, None),
            make_event("search", EventOutcome::Success, None),
            make_event("recall", EventOutcome::Success, Some(id)),
        ];

        let mut s1 = fold.init(&ctx);
        for e in &events {
            s1 = fold.reduce(s1, e, &ctx);
        }

        let mut s2 = fold.init(&ctx);
        for e in &events {
            s2 = fold.reduce(s2, e, &ctx);
        }

        let snap1 = s1.to_snapshot();
        let snap2 = s2.to_snapshot();
        assert_eq!(snap1.total_events, snap2.total_events);
        assert_eq!(snap1.relevance, snap2.relevance);
        assert_eq!(snap1.entity_posteriors, snap2.entity_posteriors);
    }
}
