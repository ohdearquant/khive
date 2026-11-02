//! Per-entity posterior evidence comes only from judgments.
//!
//! A recall hit records that a record was served and a note access records
//! that it was read; neither is a judgment of the record. A recall hit still
//! feeds the entity-blind relevance and temporal posteriors, a note access
//! feeds no posterior, and neither creates or moves a per-entity posterior.
//! Explicit and semantic feedback do.

use khive_brain_core::{
    BalancedRecallState, BetaPosterior, BrainSignal, FeedbackEventKind, FeedbackSignal,
    ServeAttribution,
};
use uuid::Uuid;

const ENTITY_CAPACITY: usize = 16;

fn recall_hit(target_id: Uuid, latency_us: i64) -> BrainSignal {
    BrainSignal::RecallHit {
        target_id,
        latency_us,
        served_by_profile_id: None,
        serve_attribution: ServeAttribution::Unspecified,
    }
}

fn feedback(target_id: Uuid, signal: FeedbackSignal) -> BrainSignal {
    BrainSignal::Feedback {
        target_id,
        signal,
        served_by_profile_id: None,
        section_signals: None,
    }
}

fn semantic(target_id: Uuid, event_kind: FeedbackEventKind, weight: f64) -> BrainSignal {
    BrainSignal::SemanticFeedback {
        target_id,
        event_kind,
        served_by_profile_id: None,
        section_signals: None,
        effective_weight: weight,
    }
}

fn assert_beta(label: &str, posterior: &BetaPosterior, alpha: f64, beta: f64) {
    assert_eq!(
        (posterior.alpha(), posterior.beta()),
        (alpha, beta),
        "{label} posterior"
    );
}

#[test]
fn recall_without_judgment_leaves_entity_posteriors_unchanged() {
    let target = Uuid::from_u128(1);

    // A fast hit (latency <= 50_000 us) adds one success to the relevance and
    // temporal posteriors: relevance Beta(7, 3) -> Beta(8, 3), temporal
    // Beta(1, 9) -> Beta(2, 9). Salience is judgment-only and stays Beta(2, 8).
    let mut fast = BalancedRecallState::new(ENTITY_CAPACITY);
    fast.apply_signal(&recall_hit(target, 10_000));
    assert!(fast.entity_posteriors.is_empty());
    assert!(fast.entity_posteriors.get(&target).is_none());
    assert_beta("relevance", &fast.relevance, 8.0, 3.0);
    assert_beta("temporal", &fast.temporal, 2.0, 9.0);
    assert_beta("salience", &fast.salience, 2.0, 8.0);
    assert_eq!(fast.total_events, 1);

    // A slow hit (latency > 50_000 us) adds one success to relevance and one
    // failure to temporal: relevance Beta(8, 3), temporal Beta(1, 10).
    let mut slow = BalancedRecallState::new(ENTITY_CAPACITY);
    slow.apply_signal(&recall_hit(target, 50_001));
    assert!(slow.entity_posteriors.is_empty());
    assert!(slow.entity_posteriors.get(&target).is_none());
    assert_beta("relevance", &slow.relevance, 8.0, 3.0);
    assert_beta("temporal", &slow.temporal, 1.0, 10.0);
    assert_beta("salience", &slow.salience, 2.0, 8.0);
    assert_eq!(slow.total_events, 1);
}

#[test]
fn note_accessed_leaves_all_posteriors_unchanged() {
    let judged = Uuid::from_u128(2);
    let other = Uuid::from_u128(3);

    // Seed one judged entity so the entity map is not trivially empty.
    let mut state = BalancedRecallState::new(ENTITY_CAPACITY);
    state.apply_signal(&feedback(judged, FeedbackSignal::Useful));
    assert_eq!(state.total_events, 1);

    let relevance = state.relevance.clone();
    let salience = state.salience.clone();
    let temporal = state.temporal.clone();
    let entities = state.entity_posteriors.to_snapshot();
    assert_eq!(entities.len(), 1);

    state.apply_signal(&BrainSignal::NoteAccessed { target_id: judged });
    state.apply_signal(&BrainSignal::NoteAccessed { target_id: other });

    assert_eq!(state.relevance, relevance);
    assert_eq!(state.salience, salience);
    assert_eq!(state.temporal, temporal);
    assert_eq!(state.entity_posteriors.to_snapshot(), entities);
    assert!(state.entity_posteriors.get(&other).is_none());
    // Only the event counter moves: one per applied signal.
    assert_eq!(state.total_events, 3);
}

#[test]
fn judgment_still_moves_entity_posterior() {
    let target = Uuid::from_u128(4);

    // Explicit feedback: a new entity starts at Beta(1, 1); Useful adds one
    // success, any other signal adds one failure. Salience moves the same way
    // from Beta(2, 8). Relevance and temporal do not move.
    let mut useful = BalancedRecallState::new(ENTITY_CAPACITY);
    useful.apply_signal(&feedback(target, FeedbackSignal::Useful));
    assert_beta(
        "entity",
        useful.entity_posteriors.get(&target).expect("entity"),
        2.0,
        1.0,
    );
    assert_beta("salience", &useful.salience, 3.0, 8.0);
    assert_beta("relevance", &useful.relevance, 7.0, 3.0);
    assert_beta("temporal", &useful.temporal, 1.0, 9.0);

    for negative in [FeedbackSignal::NotUseful, FeedbackSignal::Wrong] {
        let mut state = BalancedRecallState::new(ENTITY_CAPACITY);
        state.apply_signal(&feedback(target, negative));
        assert_beta(
            "entity",
            state.entity_posteriors.get(&target).expect("entity"),
            1.0,
            2.0,
        );
        assert_beta("salience", &state.salience, 2.0, 9.0);
    }

    // Semantic feedback with a positive weight adds that weight on the side
    // given by the event kind. An explicit event weighs 1.5.
    let mut positive = BalancedRecallState::new(ENTITY_CAPACITY);
    positive.apply_signal(&semantic(target, FeedbackEventKind::ExplicitPositive, 1.5));
    assert_beta(
        "entity",
        positive.entity_posteriors.get(&target).expect("entity"),
        2.5,
        1.0,
    );
    assert_beta("salience", &positive.salience, 3.5, 8.0);
    assert_beta("relevance", &positive.relevance, 7.0, 3.0);

    let mut negative = BalancedRecallState::new(ENTITY_CAPACITY);
    negative.apply_signal(&semantic(target, FeedbackEventKind::ExplicitNegative, 1.5));
    assert_beta(
        "entity",
        negative.entity_posteriors.get(&target).expect("entity"),
        1.0,
        2.5,
    );
    assert_beta("salience", &negative.salience, 2.0, 9.5);
    assert_beta("relevance", &negative.relevance, 7.0, 3.0);
}
