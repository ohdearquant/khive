use khive_brain_core::{BalancedRecallState, BetaPosterior};
use khive_pack_memory::recall_feedback::on_explicit_feedback;
use uuid::Uuid;

#[test]
fn recognized_memory_feedback_preserves_literal_full_snapshots() {
    let id = Uuid::from_u128(1);
    let cases = [
        ("useful", true, 1.0, false),
        ("not_useful", false, 1.0, false),
        ("wrong", false, 1.0, false),
        ("explicit_positive", true, 1.5, false),
        ("explicit_negative", false, 1.5, false),
        ("implicit_positive", true, 0.1, false),
        ("implicit_negative", false, 0.1, false),
        ("correction", false, 2.0, true),
    ];
    for (signal, positive, weight, correction) in cases {
        let mut state = BalancedRecallState::new(2);
        state.total_events = 11;
        state.exploration_epoch = 7;
        let mut expected = state.to_snapshot();
        expected.salience = if positive {
            BetaPosterior::new(2.0 + weight, 8.0)
        } else {
            BetaPosterior::new(2.0, 8.0 + weight)
        };
        if correction {
            expected.relevance = BetaPosterior::new(7.0, 5.0);
        }
        expected.entity_posteriors.insert(
            id,
            if positive {
                BetaPosterior::new(1.0 + weight, 1.0)
            } else {
                BetaPosterior::new(1.0, 1.0 + weight)
            },
        );
        expected.entity_posterior_order.push(id);
        expected.total_events += 1;
        on_explicit_feedback(&mut state, id, signal);
        assert_eq!(state.to_snapshot(), expected, "{signal}");
    }
}

#[test]
fn memory_feedback_mixed_sequence_preserves_eviction_order() {
    let [a, b, c] = [1, 2, 3].map(Uuid::from_u128);
    let mut state = BalancedRecallState::new(2);
    state.exploration_epoch = 7;
    let mut expected = state.to_snapshot();
    on_explicit_feedback(&mut state, a, "useful");
    on_explicit_feedback(&mut state, b, "not_useful");
    on_explicit_feedback(&mut state, a, "correction");
    on_explicit_feedback(&mut state, c, "explicit_positive");
    expected.relevance = BetaPosterior::new(7.0, 5.0);
    expected.salience = BetaPosterior::new(4.5, 11.0);
    expected
        .entity_posteriors
        .insert(a, BetaPosterior::new(2.0, 3.0));
    expected
        .entity_posteriors
        .insert(c, BetaPosterior::new(2.5, 1.0));
    expected.entity_posterior_order = vec![a, c];
    expected.total_events = 4;
    assert_eq!(state.to_snapshot(), expected);
}

#[test]
fn unknown_memory_feedback_preserves_populated_snapshot() {
    let [a, b, c] = [1, 2, 3].map(Uuid::from_u128);
    let mut state = BalancedRecallState::new(2);
    on_explicit_feedback(&mut state, a, "useful");
    on_explicit_feedback(&mut state, b, "correction");
    let before = state.to_snapshot();
    for signal in ["", "bad_value", "Useful", "explicit_positive "] {
        on_explicit_feedback(&mut state, c, signal);
        assert_eq!(state.to_snapshot(), before);
    }
}

#[cfg(debug_assertions)]
#[test]
fn memory_feedback_overflow_keeps_full_updated_snapshot() {
    let [a, b, c] = [1, 2, 3].map(Uuid::from_u128);
    let mut state = BalancedRecallState::new(2);
    on_explicit_feedback(&mut state, a, "useful");
    on_explicit_feedback(&mut state, b, "useful");
    state.total_events = u64::MAX;
    state.exploration_epoch = 9;
    let mut expected = state.to_snapshot();
    expected.relevance = BetaPosterior::new(7.0, 5.0);
    expected.salience = BetaPosterior::new(4.0, 10.0);
    expected.entity_posteriors.remove(&a);
    expected
        .entity_posteriors
        .insert(c, BetaPosterior::new(1.0, 3.0));
    expected.entity_posterior_order = vec![b, c];
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        on_explicit_feedback(&mut state, c, "correction");
    }));
    assert!(panic.is_err());
    assert_eq!(state.to_snapshot(), expected);
}
