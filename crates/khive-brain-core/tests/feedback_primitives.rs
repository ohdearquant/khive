use std::collections::{BTreeMap, HashMap};

use khive_brain_core::{
    BalancedRecallSnapshot, BalancedRecallState, BetaPosterior, BrainSignal, FeedbackEventKind,
    FeedbackSignal, SectionPosteriorState, SectionType, ServeAttribution,
};
use serde_json::{json, Value};
use uuid::Uuid;

fn legacy(target_id: Uuid, signal: FeedbackSignal) -> BrainSignal {
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
        effective_weight: weight,
        served_by_profile_id: None,
        section_signals: None,
    }
}

fn section_bits(state: &SectionPosteriorState) -> Value {
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

#[test]
fn feedback_primitive_preserves_full_snapshots_without_bookkeeping() {
    let id = Uuid::from_u128(1);
    let cases = [
        (legacy(id, FeedbackSignal::Useful), true, 1.0, false),
        (legacy(id, FeedbackSignal::NotUseful), false, 1.0, false),
        (legacy(id, FeedbackSignal::Wrong), false, 1.0, false),
        (
            semantic(id, FeedbackEventKind::ExplicitPositive, 1.5),
            true,
            1.5,
            false,
        ),
        (
            semantic(id, FeedbackEventKind::ExplicitNegative, 1.5),
            false,
            1.5,
            false,
        ),
        (
            semantic(id, FeedbackEventKind::ImplicitPositive, 0.1),
            true,
            0.1,
            false,
        ),
        (
            semantic(id, FeedbackEventKind::ImplicitNegative, 0.1),
            false,
            0.1,
            false,
        ),
        (
            semantic(id, FeedbackEventKind::Correction, 0.25),
            false,
            0.25,
            true,
        ),
    ];
    for (signal, positive, weight, correction) in cases {
        let mut state = BalancedRecallState::new(2);
        state.total_events = 41;
        state.exploration_epoch = 9;
        let mut expected = state.to_snapshot();
        expected.salience = if positive {
            BetaPosterior::new(2.0 + weight, 8.0)
        } else {
            BetaPosterior::new(2.0, 8.0 + weight)
        };
        if correction {
            expected.relevance = BetaPosterior::new(7.0, 3.0 + weight);
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
        state.apply_feedback_posteriors(&signal);
        assert_eq!(state.to_snapshot(), expected, "{signal:?}");
    }
}

#[test]
fn core_feedback_delegates_once_and_keeps_recall_entity_updates() {
    let [a, b, c] = [1, 2, 3].map(Uuid::from_u128);
    let mut state = BalancedRecallState::new(2);
    state.exploration_epoch = 5;
    state.apply_signal(&BrainSignal::RecallHit {
        target_id: a,
        latency_us: 50_000,
        served_by_profile_id: None,
        serve_attribution: ServeAttribution::Unspecified,
    });
    state.apply_signal(&legacy(b, FeedbackSignal::Useful));
    state.apply_signal(&semantic(a, FeedbackEventKind::Correction, 2.0));
    state.apply_signal(&legacy(c, FeedbackSignal::Wrong));
    assert_eq!(
        state.to_snapshot(),
        BalancedRecallSnapshot {
            relevance: BetaPosterior::new(8.0, 5.0),
            salience: BetaPosterior::new(3.0, 11.0),
            temporal: BetaPosterior::new(2.0, 9.0),
            entity_posteriors: HashMap::from([
                (a, BetaPosterior::new(2.0, 3.0)),
                (c, BetaPosterior::new(1.0, 2.0)),
            ]),
            entity_posteriors_version: 1,
            entity_posterior_order: vec![a, c],
            total_events: 4,
            exploration_epoch: 5,
        }
    );
}

#[test]
fn zero_weight_feedback_counts_without_touching_or_evicting_entities() {
    let [a, b, c] = [1, 2, 3].map(Uuid::from_u128);
    for weight in [0.0, -1.0, f64::NAN] {
        for target in [a, c] {
            let mut state = BalancedRecallState::new(2);
            state.apply_signal(&legacy(a, FeedbackSignal::Useful));
            state.apply_signal(&legacy(b, FeedbackSignal::Wrong));
            let baseline = state.to_snapshot();
            let signal = semantic(target, FeedbackEventKind::Correction, weight);
            state.apply_feedback_posteriors(&signal);
            assert_eq!(state.to_snapshot(), baseline);
            let mut expected = baseline;
            expected.total_events += 1;
            state.apply_signal(&signal);
            assert_eq!(state.to_snapshot(), expected);
        }
    }
}

#[cfg(debug_assertions)]
#[test]
fn core_counter_overflow_precedes_feedback_posterior_mutation() {
    let mut state = BalancedRecallState::new(2);
    state.total_events = u64::MAX;
    let before = state.to_snapshot();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.apply_signal(&semantic(
            Uuid::from_u128(1),
            FeedbackEventKind::Correction,
            2.0,
        ));
    }));
    assert!(panic.is_err());
    assert_eq!(state.to_snapshot(), before);
}

#[test]
fn section_entry_updates_full_snapshot_without_bookkeeping() {
    for signal in [
        FeedbackSignal::Useful,
        FeedbackSignal::NotUseful,
        FeedbackSignal::Wrong,
    ] {
        for weight in [0.1, 1.0, 1.5, 2.0] {
            let mut state = SectionPosteriorState::new();
            state.total_events = 41;
            state.exploration_epoch = 9;
            let mut expected = SectionPosteriorState::from_snapshot(state.to_snapshot());
            expected.posteriors.insert(
                SectionType::Overview,
                match signal {
                    FeedbackSignal::Useful => BetaPosterior::new(2.0 + weight, 2.0),
                    FeedbackSignal::NotUseful => BetaPosterior::new(2.0, 2.0 + weight),
                    FeedbackSignal::Wrong => BetaPosterior::new(2.0, 2.0 + 2.0 * weight),
                },
            );
            state
                .apply_section_feedback_entry(&SectionType::Overview, &signal, weight)
                .unwrap();
            assert_eq!(section_bits(&state), section_bits(&expected));
        }
    }
}

#[test]
fn section_entry_preserves_exact_cap_error_and_missing_row_policy() {
    let mut state = SectionPosteriorState::new();
    state
        .priors
        .insert(SectionType::Overview, BetaPosterior::new(60.0, 40.0));
    state
        .posteriors
        .insert(SectionType::Overview, BetaPosterior::new(80.0, 30.0));
    let mut expected = SectionPosteriorState::from_snapshot(state.to_snapshot());
    expected
        .posteriors
        .insert(SectionType::Overview, BetaPosterior::new(81.0, 30.0));
    let error = state
        .apply_section_feedback_entry(&SectionType::Overview, &FeedbackSignal::Useful, 1.0)
        .unwrap_err();
    assert_eq!(error, "apply_ess_cap: cap (100) must be > prior ESS (100)");
    assert_eq!(section_bits(&state), section_bits(&expected));

    state.posteriors.remove(&SectionType::Overview);
    let before = section_bits(&state);
    state
        .apply_section_feedback_entry(&SectionType::Overview, &FeedbackSignal::Useful, 0.0)
        .unwrap();
    assert_eq!(section_bits(&state), before);

    state.priors.remove(&SectionType::Examples);
    let mut expected = SectionPosteriorState::from_snapshot(state.to_snapshot());
    expected
        .posteriors
        .insert(SectionType::Examples, BetaPosterior::new(5.0, 4.0));
    state
        .apply_section_feedback_entry(&SectionType::Examples, &FeedbackSignal::Wrong, 1.0)
        .unwrap();
    assert_eq!(section_bits(&state), section_bits(&expected));
}

#[test]
fn core_section_weighting_preserves_full_snapshot_and_zero_weight_rules() {
    let sections = HashMap::from([
        (SectionType::Overview, FeedbackSignal::Wrong),
        (SectionType::Examples, FeedbackSignal::Useful),
    ]);
    let mut state = SectionPosteriorState::new();
    let mut expected = SectionPosteriorState::from_snapshot(state.to_snapshot());
    expected
        .posteriors
        .insert(SectionType::Overview, BetaPosterior::new(2.0, 5.0));
    expected
        .posteriors
        .insert(SectionType::Examples, BetaPosterior::new(6.5, 2.0));
    expected.total_events = 1;
    expected.exploration_epoch -= 1;
    let mut signal = semantic(Uuid::from_u128(1), FeedbackEventKind::ExplicitNegative, 1.5);
    if let BrainSignal::SemanticFeedback {
        section_signals, ..
    } = &mut signal
    {
        *section_signals = Some(sections);
    }
    state.apply_signal(&signal);
    assert_eq!(section_bits(&state), section_bits(&expected));
    for weight in [0.0, -1.0, f64::NAN] {
        if let BrainSignal::SemanticFeedback {
            effective_weight, ..
        } = &mut signal
        {
            *effective_weight = weight;
        }
        let before = section_bits(&state);
        state.apply_signal(&signal);
        assert_eq!(section_bits(&state), before);
    }
}

#[cfg(debug_assertions)]
#[test]
fn core_section_counter_overflow_precedes_entry_update() {
    let mut state = SectionPosteriorState::new();
    state.total_events = u64::MAX;
    let before = section_bits(&state);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.apply_section_signals(&HashMap::from([(
            SectionType::Overview,
            FeedbackSignal::Wrong,
        )]));
    }));
    assert!(panic.is_err());
    assert_eq!(section_bits(&state), before);
}
