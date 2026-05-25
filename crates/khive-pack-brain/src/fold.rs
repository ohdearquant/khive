use khive_fold::{Fold, FoldContext};
use khive_storage::event::Event;

use crate::event::{entity_signal, interpret, is_recall_positive};
use crate::state::{BetaPosterior, BrainState};

/// The brain as a meta-fold: `Fold<Event, BrainState>`.
///
/// Processes the existing Event substrate stream. Each event is interpreted
/// via `event::interpret()` and routed to the relevant posteriors.
/// Deterministic: same events in the same order → same BrainState.
pub struct EventFold {
    entity_capacity: usize,
}

impl EventFold {
    pub fn new(entity_capacity: usize) -> Self {
        Self { entity_capacity }
    }
}

impl Fold<Event, BrainState> for EventFold {
    fn init(&self, _context: &FoldContext) -> BrainState {
        BrainState::new(
            [
                (
                    "recall::relevance_weight".into(),
                    BetaPosterior::new(7.0, 3.0),
                ),
                (
                    "recall::importance_weight".into(),
                    BetaPosterior::new(2.0, 8.0),
                ),
                (
                    "recall::temporal_weight".into(),
                    BetaPosterior::new(1.0, 9.0),
                ),
            ]
            .into_iter()
            .collect(),
            self.entity_capacity,
        )
    }

    fn reduce(&self, mut state: BrainState, event: &Event, _ctx: &FoldContext) -> BrainState {
        let signal = interpret(event);

        state.total_events += 1;

        // Global recall parameter updates
        if let Some(positive) = is_recall_positive(&signal) {
            if let Some(posterior) = state.parameters.get_mut("recall::relevance_weight") {
                if positive {
                    posterior.update_success();
                } else {
                    posterior.update_failure();
                }
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

    fn finalize(&self, state: BrainState, _context: &FoldContext) -> BrainState {
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
    fn initial_state_has_recall_priors() {
        let fold = EventFold::new(100);
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);
        assert!(state.parameters.contains_key("recall::relevance_weight"));
        let p = &state.parameters["recall::relevance_weight"];
        assert!((p.alpha - 7.0).abs() < 1e-12);
        assert!((p.beta - 3.0).abs() < 1e-12);
    }

    #[test]
    fn recall_hit_updates_global_and_entity() {
        let fold = EventFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let id = Uuid::new_v4();
        let event = make_event("recall", EventOutcome::Success, Some(id));
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        let p = &state.parameters["recall::relevance_weight"];
        assert!((p.alpha - 8.0).abs() < 1e-12); // 7 + 1 success
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha - 2.0).abs() < 1e-12); // 1 + 1 success
    }

    #[test]
    fn recall_miss_updates_global_only() {
        let fold = EventFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let event = make_event("recall", EventOutcome::Success, None);
        state = fold.reduce(state, &event, &ctx);

        let p = &state.parameters["recall::relevance_weight"];
        assert!((p.beta - 4.0).abs() < 1e-12); // 3 + 1 failure
        assert!(state.entity_posteriors.is_empty());
    }

    #[test]
    fn irrelevant_event_increments_counter_only() {
        let fold = EventFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let event = make_event("link", EventOutcome::Success, Some(Uuid::new_v4()));
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        let p = &state.parameters["recall::relevance_weight"];
        assert!((p.alpha - 7.0).abs() < 1e-12); // unchanged
    }

    #[test]
    fn feedback_not_useful_increments_entity_beta() {
        let fold = EventFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let id = Uuid::new_v4();
        let mut event = make_event("brain.emit", EventOutcome::Success, Some(id));
        event.payload = serde_json::json!({"signal": "not_useful"});
        state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.total_events, 1);
        let ep = state.entity_posteriors.get(&id).unwrap();
        // default prior Beta(1,1); not_useful → update_failure → beta = 2
        assert!((ep.alpha - 1.0).abs() < 1e-12);
        assert!((ep.beta - 2.0).abs() < 1e-12);
    }

    #[test]
    fn deterministic_replay() {
        let fold = EventFold::new(100);
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
        assert_eq!(snap1.parameters, snap2.parameters);
        assert_eq!(snap1.entity_posteriors, snap2.entity_posteriors);
    }
}
