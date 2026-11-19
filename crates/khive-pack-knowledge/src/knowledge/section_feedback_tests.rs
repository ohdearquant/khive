use super::*;
use std::collections::{BTreeMap, HashMap};

use khive_brain_core::{BetaPosterior, DEFAULT_ESS_CAP};
use serde_json::{json, Value};

fn snapshot_bits(state: &SectionPosteriorState) -> Value {
    let bits = |map: &HashMap<SectionType, BetaPosterior>| {
        map.iter()
            .map(|(key, value)| {
                (
                    key.as_str(),
                    (value.alpha().to_bits(), value.beta().to_bits()),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    json!({
        "posteriors": bits(&state.posteriors), "priors": bits(&state.priors),
        "total_events": state.total_events, "exploration_epoch": state.exploration_epoch,
    })
}

// Frozen pre-extraction loop: repeated keys and per-entry capping are observable.
fn original_loop(state: &mut SectionPosteriorState, signals: &[(SectionType, FeedbackSignal)]) {
    state.total_events += 1;
    for (section_type, signal) in signals {
        if let Some(posterior) = state.posteriors.get_mut(section_type) {
            match signal {
                FeedbackSignal::Useful => posterior.update_success(),
                FeedbackSignal::NotUseful => posterior.update_failure(),
                FeedbackSignal::Wrong => posterior.update_failure_weighted(2.0),
            }
            if let Some(prior) = state.priors.get(section_type).cloned() {
                let _ = posterior.apply_ess_cap(&prior, DEFAULT_ESS_CAP);
            }
        }
    }
    if state.exploration_epoch > 0 {
        state.exploration_epoch -= 1;
    }
}

#[test]
fn ordered_duplicate_sections_keep_per_entry_caps_and_epoch() {
    let mut state = SectionPosteriorState::new();
    state
        .posteriors
        .insert(SectionType::Overview, BetaPosterior::new(49.0, 50.5));
    let before = state.to_snapshot();
    let signals = [
        (SectionType::Overview, FeedbackSignal::Useful),
        (SectionType::Overview, FeedbackSignal::Wrong),
        (SectionType::Examples, FeedbackSignal::NotUseful),
        (SectionType::Overview, FeedbackSignal::Useful),
        (SectionType::Overview, FeedbackSignal::Wrong),
    ];
    let mut expected = SectionPosteriorState::from_snapshot(before.clone());
    original_loop(&mut expected, &signals);
    on_section_feedback(&mut state, &signals);
    assert_eq!(snapshot_bits(&state), snapshot_bits(&expected));

    let mut reversed = SectionPosteriorState::from_snapshot(before.clone());
    let reversed_signals: Vec<_> = signals.iter().rev().cloned().collect();
    original_loop(&mut reversed, &reversed_signals);
    assert_ne!(
        snapshot_bits(&state),
        snapshot_bits(&reversed),
        "fixture must expose slice order"
    );

    let mut collapsed = SectionPosteriorState::from_snapshot(before.clone());
    original_loop(
        &mut collapsed,
        &[
            (SectionType::Overview, FeedbackSignal::Wrong),
            (SectionType::Examples, FeedbackSignal::NotUseful),
        ],
    );
    assert_ne!(
        snapshot_bits(&state),
        snapshot_bits(&collapsed),
        "fixture must expose repeated keys"
    );

    let mut deferred = SectionPosteriorState::from_snapshot(before);
    for (section, signal) in &signals {
        let posterior = deferred.posteriors.get_mut(section).unwrap();
        match signal {
            FeedbackSignal::Useful => posterior.update_success(),
            FeedbackSignal::NotUseful => posterior.update_failure(),
            FeedbackSignal::Wrong => posterior.update_failure_weighted(2.0),
        }
    }
    deferred
        .posteriors
        .get_mut(&SectionType::Overview)
        .unwrap()
        .apply_ess_cap(&deferred.priors[&SectionType::Overview], DEFAULT_ESS_CAP)
        .unwrap();
    deferred.total_events += 1;
    deferred.exploration_epoch -= 1;
    assert_ne!(
        snapshot_bits(&state),
        snapshot_bits(&deferred),
        "fixture must expose per-entry capping"
    );
}

#[test]
fn knowledge_section_bookkeeping_and_missing_rows_keep_old_policy() {
    for epoch in [0, 1, 50] {
        for signals in [
            vec![],
            vec![
                (SectionType::Overview, FeedbackSignal::Useful),
                (SectionType::Examples, FeedbackSignal::Wrong),
            ],
        ] {
            let mut state = SectionPosteriorState::new();
            state.exploration_epoch = epoch;
            state.posteriors.remove(&SectionType::Overview);
            state.priors.remove(&SectionType::Examples);
            let mut expected = SectionPosteriorState::from_snapshot(state.to_snapshot());
            original_loop(&mut expected, &signals);
            on_section_feedback(&mut state, &signals);
            assert_eq!(snapshot_bits(&state), snapshot_bits(&expected));
        }
    }
}

#[test]
fn knowledge_cap_error_preserves_updated_row_and_continues() {
    let mut state = SectionPosteriorState::new();
    state
        .priors
        .insert(SectionType::Overview, BetaPosterior::new(60.0, 40.0));
    state
        .posteriors
        .insert(SectionType::Overview, BetaPosterior::new(80.0, 30.0));
    let signals = [
        (SectionType::Overview, FeedbackSignal::Useful),
        (SectionType::Examples, FeedbackSignal::Wrong),
    ];
    let mut expected = SectionPosteriorState::from_snapshot(state.to_snapshot());
    original_loop(&mut expected, &signals);
    on_section_feedback(&mut state, &signals);
    assert_eq!(snapshot_bits(&state), snapshot_bits(&expected));
    assert_eq!(
        state.posteriors[&SectionType::Overview],
        BetaPosterior::new(81.0, 30.0)
    );
    assert_eq!(
        state.posteriors[&SectionType::Examples],
        BetaPosterior::new(5.0, 4.0)
    );
}

#[cfg(debug_assertions)]
#[test]
fn knowledge_counter_overflow_precedes_section_update() {
    let mut state = SectionPosteriorState::new();
    state.total_events = u64::MAX;
    let before = snapshot_bits(&state);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        on_section_feedback(
            &mut state,
            &[(SectionType::Overview, FeedbackSignal::Wrong)],
        );
    }));
    assert!(panic.is_err());
    assert_eq!(snapshot_bits(&state), before);
}
