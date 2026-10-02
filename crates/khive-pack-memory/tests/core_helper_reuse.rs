use khive_brain_core::{BalancedRecallState, BetaPosterior};
#[cfg(debug_assertions)]
use khive_pack_memory::recall_feedback::on_explicit_feedback;
use khive_pack_memory::recall_feedback::{on_recall_hit, on_recall_miss};
use khive_pack_memory::scoring::{
    contains_cjk, is_cjk_char, AdjustmentCondition, CandidateContext,
};
use uuid::Uuid;

#[test]
fn recall_hit_preserves_temporal_boundary_and_entity_state() {
    let id = Uuid::from_u128(1);
    for (latency, temporal) in [
        (-1, (2.0, 9.0)),
        (49_999, (2.0, 9.0)),
        (50_000, (2.0, 9.0)),
        (50_001, (1.0, 10.0)),
    ] {
        let mut state = BalancedRecallState::new(2);
        state.exploration_epoch = 7;
        on_recall_hit(&mut state, id, latency);
        let snapshot = state.to_snapshot();
        assert_eq!(snapshot.relevance, BetaPosterior::new(8.0, 3.0));
        assert_eq!(snapshot.salience, BetaPosterior::new(2.0, 8.0));
        assert_eq!(
            snapshot.temporal,
            BetaPosterior::new(temporal.0, temporal.1)
        );
        assert_eq!(snapshot.entity_posteriors.len(), 1);
        assert_eq!(
            snapshot.entity_posteriors[&id],
            BetaPosterior::new(2.0, 1.0)
        );
        assert_eq!(snapshot.entity_posterior_order, vec![id]);
        assert_eq!(snapshot.entity_posteriors_version, 1);
        assert_eq!(snapshot.total_events, 1);
        assert_eq!(snapshot.exploration_epoch, 7);
    }
}

#[test]
fn recall_miss_preserves_entity_eviction_order_and_exploration() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let c = Uuid::from_u128(3);
    let mut state = BalancedRecallState::new(2);
    state.exploration_epoch = 7;
    on_recall_hit(&mut state, a, 50_000);
    on_recall_hit(&mut state, b, 50_001);
    on_recall_hit(&mut state, a, -1);
    assert_eq!(state.entity_posteriors.order(), vec![b, a]);
    on_recall_hit(&mut state, c, 50_000);
    let before_miss = state.to_snapshot();
    assert_eq!(before_miss.entity_posterior_order, vec![a, c]);
    assert_eq!(before_miss.entity_posteriors.len(), 2);
    assert!(!before_miss.entity_posteriors.contains_key(&b));
    assert_eq!(
        before_miss.entity_posteriors[&a],
        BetaPosterior::new(3.0, 1.0)
    );
    assert_eq!(
        before_miss.entity_posteriors[&c],
        BetaPosterior::new(2.0, 1.0)
    );
    on_recall_miss(&mut state);
    let after_miss = state.to_snapshot();
    assert_eq!(after_miss.relevance, BetaPosterior::new(11.0, 4.0));
    assert_eq!(after_miss.salience, BetaPosterior::new(2.0, 8.0));
    assert_eq!(after_miss.temporal, BetaPosterior::new(4.0, 11.0));
    assert_eq!(after_miss.entity_posteriors, before_miss.entity_posteriors);
    assert_eq!(after_miss.entity_posterior_order, vec![a, c]);
    assert_eq!(after_miss.total_events, 5);
    assert_eq!(after_miss.exploration_epoch, 7);
}

#[cfg(debug_assertions)]
#[test]
fn explicit_feedback_retains_posterior_updates_before_counter_overflow() {
    let id = Uuid::from_u128(1);
    let mut state = BalancedRecallState::new(2);
    state.total_events = u64::MAX;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        on_explicit_feedback(&mut state, id, "correction");
    }));
    assert!(result.is_err());
    assert_eq!(state.total_events, u64::MAX);
    assert_eq!(state.relevance, BetaPosterior::new(7.0, 5.0));
    assert_eq!(state.salience, BetaPosterior::new(2.0, 10.0));
    assert_eq!(state.temporal, BetaPosterior::new(1.0, 9.0));
    assert_eq!(
        state.entity_posteriors.get(&id),
        Some(&BetaPosterior::new(1.0, 3.0))
    );
}

#[test]
fn memory_cjk_public_import_preserves_all_supported_block_boundaries() {
    for c in [
        '\u{3040}',
        '\u{309f}',
        '\u{30a0}',
        '\u{30ff}',
        '\u{3400}',
        '\u{4dbf}',
        '\u{4e00}',
        '\u{9fff}',
        '\u{f900}',
        '\u{faff}',
        '\u{ac00}',
        '\u{d7af}',
        '\u{20000}',
        '\u{2a6df}',
    ] {
        assert!(is_cjk_char(c), "missing CJK boundary {c:?}");
    }
    for c in [
        '\u{303f}',
        '\u{3100}',
        '\u{33ff}',
        '\u{4dc0}',
        '\u{4dff}',
        '\u{a000}',
        '\u{f8ff}',
        '\u{fb00}',
        '\u{abff}',
        '\u{d7b0}',
        '\u{1ffff}',
        '\u{2a6e0}',
        'a',
        '🦀',
    ] {
        assert!(!is_cjk_char(c), "unexpected CJK boundary {c:?}");
    }
}

#[test]
fn cjk_classification_drives_memory_entity_matching_and_routing() {
    let names = vec!["\u{20000}\u{20001}".to_owned()];
    let ctx = CandidateContext {
        memory_type: "semantic",
        age_days: 0.0,
        salience: 0.5,
        content: "x\u{20000}\u{20001}y",
        entity_names: &names,
    };
    assert!(AdjustmentCondition::EntityMatch.matches(&ctx));
    assert!(!contains_cjk("hello world"));
    assert!(!contains_cjk(""));
    assert!(!contains_cjk("世界雪abcdefghijklmnopq"));
    assert!(contains_cjk("世界雪abcdefghijklmnop"));
}
