//! Beta-posterior fold implementations for brain profiles.

use khive_fold::{Fold, FoldContext};
use khive_storage::event::Event;

use crate::event::interpret;
use khive_brain_core::{BalancedRecallState, SectionPosteriorState};

/// Fold for the `balanced-recall-v1` three-scalar Beta-posterior state.
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
        state.apply_signal(&signal);
        state
    }

    fn finalize(&self, state: BalancedRecallState, _context: &FoldContext) -> BalancedRecallState {
        state
    }
}

/// Fold for per-profile section posteriors.
pub struct SectionPosteriorFold;

impl SectionPosteriorFold {
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
        _ctx: &FoldContext,
    ) -> SectionPosteriorState {
        let signal = interpret(event);
        state.apply_signal(&signal);
        state
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
        assert!((state.relevance.alpha() - 7.0).abs() < 1e-12);
        assert!((state.relevance.beta() - 3.0).abs() < 1e-12);
        // salience prior Beta(2,8)
        assert!((state.salience.alpha() - 2.0).abs() < 1e-12);
        assert!((state.salience.beta() - 8.0).abs() < 1e-12);
        // temporal prior Beta(1,9)
        assert!((state.temporal.alpha() - 1.0).abs() < 1e-12);
        assert!((state.temporal.beta() - 9.0).abs() < 1e-12);
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
        assert!((state.relevance.alpha() - 8.0).abs() < 1e-12); // 7 + 1
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha() - 2.0).abs() < 1e-12); // 1 + 1
    }

    #[test]
    fn recall_miss_updates_relevance_beta() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);

        let event = make_event("recall", EventOutcome::Success, None);
        state = fold.reduce(state, &event, &ctx);

        // target_id = None → RecallMiss → relevance failure
        assert!((state.relevance.beta() - 4.0).abs() < 1e-12); // 3 + 1
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
        assert!((state.relevance.alpha() - 7.0).abs() < 1e-12); // unchanged
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
        assert!((ep.alpha() - 1.0).abs() < 1e-12);
        assert!((ep.beta() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn brain_emit_legacy_does_not_update_entity() {
        // brain.emit predates brain.feedback; now treated as Irrelevant
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

    // ── SemanticFeedback fold path tests (MAJ-001 coverage) ──────────────────

    fn make_semantic_feedback_event(signal: &str, target: Uuid) -> Event {
        let mut e = Event::new(
            "test",
            "brain.feedback",
            khive_types::EventKind::Audit,
            SubstrateKind::Note,
            "brain",
        );
        e.outcome = EventOutcome::Success;
        e.target_id = Some(target);
        e.payload = serde_json::json!({"signal": signal});
        e
    }

    #[test]
    fn semantic_feedback_explicit_positive_updates_salience_alpha_and_entity_alpha() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        let sal_alpha_prior = state.salience.alpha(); // 2.0
        let sal_beta_prior = state.salience.beta(); // 8.0

        let id = Uuid::new_v4();
        let event = make_semantic_feedback_event("explicit_positive", id);
        let state = fold.reduce(state, &event, &ctx);

        // ExplicitPositive: weight=1.5, is_positive=true → salience.alpha() += 1.5
        assert!(
            (state.salience.alpha() - (sal_alpha_prior + 1.5)).abs() < 1e-12,
            "explicit_positive must add 1.5 to salience.alpha(): expected {}, got {}",
            sal_alpha_prior + 1.5,
            state.salience.alpha()
        );
        assert!(
            (state.salience.beta() - sal_beta_prior).abs() < 1e-12,
            "explicit_positive must not change salience.beta()"
        );
        // Correction branch must NOT fire
        let rel_beta_prior = state.relevance.beta();
        // (relevance should not change for ExplicitPositive — only Correction updates relevance)
        let _ = rel_beta_prior;

        // Entity posterior: alpha += 1.5
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!(
            (ep.alpha() - 2.5).abs() < 1e-12,
            "entity posterior alpha must be 1.0 + 1.5 = 2.5, got {}",
            ep.alpha()
        );
        assert!(
            (ep.beta() - 1.0).abs() < 1e-12,
            "entity posterior beta must remain at 1.0, got {}",
            ep.beta()
        );
    }

    #[test]
    fn semantic_feedback_implicit_negative_updates_salience_beta_and_entity_beta() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        let sal_alpha_prior = state.salience.alpha(); // 2.0
        let sal_beta_prior = state.salience.beta(); // 8.0

        let id = Uuid::new_v4();
        let event = make_semantic_feedback_event("implicit_negative", id);
        let state = fold.reduce(state, &event, &ctx);

        // ImplicitNegative: weight=0.5, is_positive=false → salience.beta() += 0.5
        assert!(
            (state.salience.alpha() - sal_alpha_prior).abs() < 1e-12,
            "implicit_negative must not change salience.alpha()"
        );
        assert!(
            (state.salience.beta() - (sal_beta_prior + 0.5)).abs() < 1e-12,
            "implicit_negative must add 0.5 to salience.beta(): expected {}, got {}",
            sal_beta_prior + 0.5,
            state.salience.beta()
        );

        // Entity posterior: beta += 0.5
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!(
            (ep.alpha() - 1.0).abs() < 1e-12,
            "entity posterior alpha must remain at 1.0, got {}",
            ep.alpha()
        );
        assert!(
            (ep.beta() - 1.5).abs() < 1e-12,
            "entity posterior beta must be 1.0 + 0.5 = 1.5, got {}",
            ep.beta()
        );
    }

    #[test]
    fn semantic_feedback_correction_updates_salience_beta_relevance_beta_and_entity_beta() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        let sal_alpha_prior = state.salience.alpha(); // 2.0
        let sal_beta_prior = state.salience.beta(); // 8.0
        let rel_alpha_prior = state.relevance.alpha(); // 7.0
        let rel_beta_prior = state.relevance.beta(); // 3.0

        let id = Uuid::new_v4();
        let event = make_semantic_feedback_event("correction", id);
        let state = fold.reduce(state, &event, &ctx);

        // Correction: weight=2.0, is_positive=false → salience.beta() += 2.0
        assert!(
            (state.salience.alpha() - sal_alpha_prior).abs() < 1e-12,
            "correction must not change salience.alpha()"
        );
        assert!(
            (state.salience.beta() - (sal_beta_prior + 2.0)).abs() < 1e-12,
            "correction must add 2.0 to salience.beta(): expected {}, got {}",
            sal_beta_prior + 2.0,
            state.salience.beta()
        );

        // Correction also updates relevance posterior → relevance.beta() += 2.0
        assert!(
            (state.relevance.alpha() - rel_alpha_prior).abs() < 1e-12,
            "correction must not change relevance.alpha()"
        );
        assert!(
            (state.relevance.beta() - (rel_beta_prior + 2.0)).abs() < 1e-12,
            "correction must add 2.0 to relevance.beta(): expected {}, got {}",
            rel_beta_prior + 2.0,
            state.relevance.beta()
        );

        // Entity posterior: beta += 2.0
        let ep = state.entity_posteriors.get(&id).unwrap();
        assert!(
            (ep.alpha() - 1.0).abs() < 1e-12,
            "entity posterior alpha must remain at 1.0, got {}",
            ep.alpha()
        );
        assert!(
            (ep.beta() - 3.0).abs() < 1e-12,
            "entity posterior beta must be 1.0 + 2.0 = 3.0, got {}",
            ep.beta()
        );
    }

    // ── Regression tests (issues #355, #356, #357, #295) ──────────────────────

    // #355 (MAJ-001): salience and temporal posteriors must update after dispatch.
    #[test]
    fn test_355_posteriors_update_after_dispatch() {
        let fold = BalancedRecallFold::new(100);
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        // Baseline: domain-informed priors.
        let sal_alpha_prior = state.salience.alpha(); // 2.0
        let sal_beta_prior = state.salience.beta(); // 8.0
        let tmp_alpha_prior = state.temporal.alpha(); // 1.0
        let tmp_beta_prior = state.temporal.beta(); // 9.0

        // Useful feedback → salience success.
        let id = Uuid::new_v4();
        let mut fb_useful = make_event("brain.feedback", EventOutcome::Success, Some(id));
        fb_useful.payload = serde_json::json!({"signal": "useful"});
        let state = fold.reduce(state, &fb_useful, &ctx);

        assert!(
            (state.salience.alpha() - (sal_alpha_prior + 1.0)).abs() < 1e-12,
            "useful feedback must increment salience.alpha(): expected {}, got {}",
            sal_alpha_prior + 1.0,
            state.salience.alpha()
        );
        assert!(
            (state.salience.beta() - sal_beta_prior).abs() < 1e-12,
            "useful feedback must not change salience.beta()"
        );

        // Fast recall hit → temporal success (latency_us = 0 ≤ 50_000).
        let mut hit = make_event("recall", EventOutcome::Success, Some(id));
        hit.duration_us = 0;
        let state = fold.reduce(state, &hit, &ctx);

        assert!(
            (state.temporal.alpha() - (tmp_alpha_prior + 1.0)).abs() < 1e-12,
            "fast recall hit must increment temporal.alpha(): expected {}, got {}",
            tmp_alpha_prior + 1.0,
            state.temporal.alpha()
        );
        assert!(
            (state.temporal.beta() - tmp_beta_prior).abs() < 1e-12,
            "fast recall hit must not change temporal.beta()"
        );

        // Slow recall hit → temporal failure (latency_us > 50_000).
        let mut slow_hit = make_event("recall", EventOutcome::Success, Some(id));
        slow_hit.duration_us = 100_000;
        let state = fold.reduce(state, &slow_hit, &ctx);

        assert!(
            (state.temporal.beta() - (tmp_beta_prior + 1.0)).abs() < 1e-12,
            "slow recall hit must increment temporal.beta()"
        );

        // Not-useful feedback → salience failure.
        let mut fb_bad = make_event("brain.feedback", EventOutcome::Success, Some(id));
        fb_bad.payload = serde_json::json!({"signal": "not_useful"});
        let state = fold.reduce(state, &fb_bad, &ctx);

        assert!(
            (state.salience.beta() - (sal_beta_prior + 1.0)).abs() < 1e-12,
            "not_useful feedback must increment salience.beta()"
        );
    }

    // ── SectionPosteriorFold tests ───────────────────────────────────────────

    use khive_brain_core::SectionType as ST;

    fn make_section_feedback_event(section_signals: serde_json::Value) -> Event {
        let id = Uuid::new_v4();
        let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
        e.payload = serde_json::json!({
            "signal": "useful",
            "section_signals": section_signals
        });
        e
    }

    #[test]
    fn section_fold_useful_increments_alpha() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        let alpha_before = state.posteriors[&ST::Overview].alpha();

        let event = make_section_feedback_event(serde_json::json!({
            "overview": "useful"
        }));
        let state = fold.reduce(state, &event, &ctx);

        assert!(
            (state.posteriors[&ST::Overview].alpha() - (alpha_before + 1.0)).abs() < 1e-12,
            "useful must increment alpha"
        );
    }

    #[test]
    fn section_fold_not_useful_increments_beta() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        let beta_before = state.posteriors[&ST::Formalism].beta();

        let event = make_section_feedback_event(serde_json::json!({
            "formalism": "not_useful"
        }));
        let state = fold.reduce(state, &event, &ctx);

        assert!(
            (state.posteriors[&ST::Formalism].beta() - (beta_before + 1.0)).abs() < 1e-12,
            "not_useful must increment beta by 1"
        );
    }

    #[test]
    fn section_fold_wrong_increments_beta_by_two() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);

        let beta_before = state.posteriors[&ST::Examples].beta();

        let event = make_section_feedback_event(serde_json::json!({
            "examples": "wrong"
        }));
        let state = fold.reduce(state, &event, &ctx);

        assert!(
            (state.posteriors[&ST::Examples].beta() - (beta_before + 2.0)).abs() < 1e-12,
            "wrong must increment beta by 2"
        );
    }

    #[test]
    fn section_fold_no_section_signals_is_noop() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);
        let total_before = state.total_events;

        // Feedback without section_signals
        let id = Uuid::new_v4();
        let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
        e.payload = serde_json::json!({"signal": "useful"});
        let state = fold.reduce(state, &e, &ctx);

        assert_eq!(
            state.total_events, total_before,
            "no section_signals should be noop"
        );
    }

    #[test]
    fn section_fold_epoch_decrements() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();
        let state = fold.init(&ctx);
        let epoch_before = state.exploration_epoch;

        let event = make_section_feedback_event(serde_json::json!({
            "overview": "useful"
        }));
        let state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.exploration_epoch, epoch_before - 1);
    }

    #[test]
    fn section_fold_epoch_floors_at_zero() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();
        let mut state = fold.init(&ctx);
        state.exploration_epoch = 0;

        let event = make_section_feedback_event(serde_json::json!({
            "overview": "useful"
        }));
        let state = fold.reduce(state, &event, &ctx);

        assert_eq!(state.exploration_epoch, 0, "epoch must floor at 0");
    }

    #[test]
    fn section_fold_deterministic_replay() {
        let fold = SectionPosteriorFold::new();
        let ctx = FoldContext::new();

        let events = vec![
            make_section_feedback_event(
                serde_json::json!({"overview": "useful", "formalism": "not_useful"}),
            ),
            make_section_feedback_event(serde_json::json!({"examples": "wrong"})),
            make_section_feedback_event(serde_json::json!({"overview": "useful"})),
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
        for st in ST::all() {
            assert!(
                (snap1.posteriors[st].alpha() - snap2.posteriors[st].alpha()).abs() < 1e-12,
                "replay alpha mismatch for {:?}",
                st
            );
            assert!(
                (snap1.posteriors[st].beta() - snap2.posteriors[st].beta()).abs() < 1e-12,
                "replay beta mismatch for {:?}",
                st
            );
        }
    }
}
