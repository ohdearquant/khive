//! Recall-domain posterior updates — direct update functions replacing fold-based replay.
//!
//! These functions mutate a `BalancedRecallState` in place at the point of action:
//! after a recall hit/miss and after explicit feedback. No fold trait or event log
//! needed — pack-memory owns its own posterior lifecycle.

use khive_brain_core::{BalancedRecallState, BetaPosterior, FeedbackEventKind, FeedbackSignal};
use uuid::Uuid;

/// Threshold below which a recall is considered "fast" for temporal posterior updates.
///
/// 50 000 µs = 50 ms. Local SQLite FTS5 completes in 1–20 ms under normal conditions;
/// 50 ms provides headroom for contention while staying below the 250 ms rerank budget.
const FAST_US: i64 = 50_000;

/// Called after a successful `memory.recall` that returned at least one result.
///
/// - `relevance`: success (a result was returned)
/// - `temporal`: success if `latency_us` ≤ 50 ms, failure otherwise
/// - per-entity posterior: success for `target_id`
pub fn on_recall_hit(state: &mut BalancedRecallState, target_id: Uuid, latency_us: i64) {
    state.total_events += 1;
    state.relevance.update_success();
    if latency_us <= FAST_US {
        state.temporal.update_success();
    } else {
        state.temporal.update_failure();
    }
    let posterior = state
        .entity_posteriors
        .get_or_insert(target_id, || BetaPosterior::new(1.0, 1.0));
    posterior.update_success();
}

/// Called after a `memory.recall` that returned no results.
///
/// - `relevance`: failure
/// - `temporal`: failure
pub fn on_recall_miss(state: &mut BalancedRecallState) {
    state.total_events += 1;
    state.relevance.update_failure();
    state.temporal.update_failure();
}

/// Called when an agent provides explicit feedback on a recalled entity.
///
/// Accepted `signal` strings (same vocabulary as `brain.feedback`):
/// - Legacy: `"useful"`, `"not_useful"`, `"wrong"`
/// - Semantic: `"explicit_positive"`, `"explicit_negative"`, `"implicit_positive"`,
///   `"implicit_negative"`, `"correction"`
///
/// Unknown signal strings are silently ignored.
pub fn on_explicit_feedback(state: &mut BalancedRecallState, target_id: Uuid, signal: &str) {
    // Try semantic event kind first (weighted updates), then legacy signal.
    if let Some(event_kind) = FeedbackEventKind::from_signal_str(signal) {
        let w = event_kind.update_weight();
        if event_kind.is_positive() {
            state.salience.update_success_weighted(w);
        } else {
            state.salience.update_failure_weighted(w);
        }
        // Corrections also penalise the relevance posterior (strongest negative signal).
        if event_kind == FeedbackEventKind::Correction {
            state.relevance.update_failure_weighted(w);
        }
        // Per-entity posterior (weighted).
        let posterior = state
            .entity_posteriors
            .get_or_insert(target_id, || BetaPosterior::new(1.0, 1.0));
        if event_kind.is_positive() {
            posterior.update_success_weighted(w);
        } else {
            posterior.update_failure_weighted(w);
        }
        state.total_events += 1;
    } else if let Ok(fb) =
        serde_json::from_value::<FeedbackSignal>(serde_json::Value::String(signal.to_owned()))
    {
        // Legacy signal: useful / not_useful / wrong
        let positive = matches!(fb, FeedbackSignal::Useful);
        if positive {
            state.salience.update_success();
        } else {
            state.salience.update_failure();
        }
        let posterior = state
            .entity_posteriors
            .get_or_insert(target_id, || BetaPosterior::new(1.0, 1.0));
        if positive {
            posterior.update_success();
        } else {
            posterior.update_failure();
        }
        state.total_events += 1;
    }
    // Unknown signal → no-op (don't poison state with bad data).
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> BalancedRecallState {
        BalancedRecallState::new(100)
    }

    #[test]
    fn recall_hit_fast_increments_relevance_temporal_and_entity_alpha() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_recall_hit(&mut s, id, 10_000); // 10 ms — fast

        assert_eq!(s.total_events, 1);
        // relevance prior Beta(7,3): alpha should become 8
        assert!((s.relevance.alpha() - 8.0).abs() < 1e-12);
        assert!((s.relevance.beta() - 3.0).abs() < 1e-12);
        // temporal prior Beta(1,9): alpha should become 2
        assert!((s.temporal.alpha() - 2.0).abs() < 1e-12);
        assert!((s.temporal.beta() - 9.0).abs() < 1e-12);
        // entity posterior: alpha 1+1=2
        let ep = s.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn recall_hit_slow_increments_relevance_alpha_but_temporal_beta() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_recall_hit(&mut s, id, 100_000); // 100 ms — slow

        assert!((s.relevance.alpha() - 8.0).abs() < 1e-12);
        // temporal failure → beta 9+1=10
        assert!((s.temporal.beta() - 10.0).abs() < 1e-12);
        assert!((s.temporal.alpha() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn recall_miss_increments_relevance_beta_and_temporal_beta() {
        let mut s = fresh();
        on_recall_miss(&mut s);

        assert_eq!(s.total_events, 1);
        assert!((s.relevance.beta() - 4.0).abs() < 1e-12); // 3+1
        assert!((s.temporal.beta() - 10.0).abs() < 1e-12); // 9+1
        assert!(s.entity_posteriors.is_empty());
    }

    #[test]
    fn explicit_feedback_useful_increments_salience_alpha() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_explicit_feedback(&mut s, id, "useful");

        assert_eq!(s.total_events, 1);
        // salience prior Beta(2,8): alpha 2+1=3
        assert!((s.salience.alpha() - 3.0).abs() < 1e-12);
        assert!((s.salience.beta() - 8.0).abs() < 1e-12);
        let ep = s.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn explicit_feedback_not_useful_increments_salience_beta() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_explicit_feedback(&mut s, id, "not_useful");

        assert!((s.salience.beta() - 9.0).abs() < 1e-12); // 8+1
        let ep = s.entity_posteriors.get(&id).unwrap();
        assert!((ep.beta() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn explicit_feedback_wrong_increments_salience_beta() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_explicit_feedback(&mut s, id, "wrong");

        assert!((s.salience.beta() - 9.0).abs() < 1e-12); // 8+1
    }

    #[test]
    fn explicit_feedback_explicit_positive_applies_weight_1_5() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_explicit_feedback(&mut s, id, "explicit_positive");

        // ExplicitPositive: weight=1.5, positive → salience.alpha() += 1.5
        assert!((s.salience.alpha() - 3.5).abs() < 1e-12); // 2+1.5
        assert!((s.salience.beta() - 8.0).abs() < 1e-12);
        let ep = s.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha() - 2.5).abs() < 1e-12); // 1+1.5
    }

    #[test]
    fn explicit_feedback_correction_updates_salience_beta_and_relevance_beta() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        on_explicit_feedback(&mut s, id, "correction");

        // Correction: weight=2.0, negative → salience.beta() += 2.0
        assert!((s.salience.beta() - 10.0).abs() < 1e-12); // 8+2
                                                           // Correction also penalises relevance → relevance.beta() += 2.0
        assert!((s.relevance.beta() - 5.0).abs() < 1e-12); // 3+2
        let ep = s.entity_posteriors.get(&id).unwrap();
        assert!((ep.beta() - 3.0).abs() < 1e-12); // 1+2
    }

    #[test]
    fn explicit_feedback_unknown_signal_is_noop() {
        let mut s = fresh();
        let id = Uuid::new_v4();
        let sal_before = (s.salience.alpha(), s.salience.beta());
        on_explicit_feedback(&mut s, id, "bad_value");

        assert_eq!(s.total_events, 0);
        assert!((s.salience.alpha() - sal_before.0).abs() < 1e-12);
        assert!((s.salience.beta() - sal_before.1).abs() < 1e-12);
        assert!(s.entity_posteriors.is_empty());
    }
}
