use super::{BalancedRecallFold, SectionPosteriorFold};
use crate::event::interpret;
use khive_brain_core::{BalancedRecallState, BetaPosterior, SectionPosteriorState, SectionType};
use khive_fold::{Fold, FoldContext};
use khive_storage::event::Event;
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

// The oracle is the pre-composition implementation from 76daddee666af4fe66a9bd7e2f56da7d1365aed8.
// Only the two fold type names differ; it must not call the new pipeline helpers.
/// Fold for the `balanced-recall-v1` three-scalar Beta-posterior state.
pub struct LegacyBalancedRecallFold {
    entity_capacity: usize,
}

impl LegacyBalancedRecallFold {
    /// Create a fold with the given entity capacity for the `balanced-recall-v1` state.
    pub fn new(entity_capacity: usize) -> Self {
        Self { entity_capacity }
    }
}

impl Fold<Event, BalancedRecallState> for LegacyBalancedRecallFold {
    fn init(&self, _context: &FoldContext) -> BalancedRecallState {
        BalancedRecallState::new(self.entity_capacity)
    }

    fn reduce(
        &self,
        mut state: BalancedRecallState,
        event: &Event,
        _ctx: &FoldContext,
    ) -> BalancedRecallState {
        // Unlike other irrelevant events, unjudged telemetry is not a training event.
        if event.kind == EventKind::FeedbackUnjudged {
            return state;
        }
        let signal = interpret(event);
        state.apply_signal(&signal);
        state
    }

    fn finalize(&self, state: BalancedRecallState, _context: &FoldContext) -> BalancedRecallState {
        state
    }
}

/// Fold for per-profile section posteriors.
pub struct LegacySectionPosteriorFold;

impl LegacySectionPosteriorFold {
    /// Create a default `LegacySectionPosteriorFold`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for LegacySectionPosteriorFold {
    fn default() -> Self {
        Self::new()
    }
}

impl Fold<Event, SectionPosteriorState> for LegacySectionPosteriorFold {
    fn init(&self, _context: &FoldContext) -> SectionPosteriorState {
        SectionPosteriorState::new()
    }

    fn reduce(
        &self,
        mut state: SectionPosteriorState,
        event: &Event,
        _ctx: &FoldContext,
    ) -> SectionPosteriorState {
        if event.verb == "brain.section_feedback" {
            if let Ok((_, signals)) = crate::section_feedback::decode_event(event) {
                if !signals.is_empty() {
                    state.apply_section_signals(&signals);
                }
            }
            return state;
        }
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

fn event(verb: &str, target: Option<u128>, payload: Value) -> Event {
    Event {
        id: Uuid::nil(),
        namespace: "fold-composition".into(),
        verb: verb.into(),
        substrate: SubstrateKind::Event,
        actor: "actor:fold-composition".into(),
        kind: EventKind::Audit,
        outcome: EventOutcome::Success,
        payload,
        payload_schema_version: 1,
        profile_state_version: None,
        duration_us: 50_000,
        target_id: target.map(Uuid::from_u128),
        session_id: None,
        aggregate_kind: None,
        aggregate_id: None,
        created_at: 0,
        op_index: None,
        ref_resolution: None,
    }
}

fn standalone(signals: Value) -> Event {
    let mut event = event(
        "brain.section_feedback",
        None,
        json!({
            "served_by_profile_id": "balanced-recall-v1",
            "section_signals": signals,
        }),
    );
    event.kind = EventKind::FeedbackExplicit;
    event
}

fn mixed_events() -> Vec<Event> {
    let mut slow = event("memory.recall", Some(2), json!({}));
    slow.duration_us = 50_001;
    let mut failed = event("recall", Some(3), json!({}));
    failed.outcome = EventOutcome::Error;
    let mut events = vec![
        event(
            "memory.recall",
            Some(1),
            json!({"served_by_profile_id": "p", "serve_attribution": "profile"}),
        ),
        slow,
        event("recall", None, json!({})),
        failed,
        event("search", None, json!({})),
        event("get", Some(3), json!({})),
        event("remember", Some(4), json!({})),
        event("unrelated", Some(1), json!({})),
        event("brain.emit", Some(2), json!({"signal": "useful"})),
        event("brain.feedback", None, json!({"signal": "useful"})),
        event("brain.feedback", Some(1), json!({"signal": "unknown"})),
    ];
    for signal in [
        "useful",
        "not_useful",
        "wrong",
        "explicit_positive",
        "explicit_negative",
        "implicit_positive",
        "implicit_negative",
        "correction",
    ] {
        events.push(event(
            "brain.feedback",
            Some(5),
            json!({
                "signal": signal,
                "section_signals": {"overview": "useful", "examples": "wrong"},
            }),
        ));
    }
    for weight in [0.0, 0.25, 1.5] {
        events.push(event(
            "brain.feedback",
            Some(6),
            json!({
                "signal": "correction",
                "gate": {"effective_weight": weight},
                "section_signals": {"overview": "not_useful"},
            }),
        ));
    }
    for verb in ["brain.feedback", "recall", "search", "get"] {
        let mut telemetry = event(
            verb,
            Some(7),
            json!({
                "signal": "explicit_positive",
                "section_signals": {"overview": "useful"},
                "gate": {"effective_weight": 100.0},
            }),
        );
        telemetry.kind = EventKind::FeedbackUnjudged;
        events.push(telemetry);
    }
    for signals in [
        json!({"overview": "useful", "references": "wrong"}),
        json!({"references": "useful", "other": "wrong"}),
        json!({"overview": "wrong", "glossary": "useful"}),
        json!({}),
        json!(["overview"]),
    ] {
        events.push(event(
            "brain.feedback",
            Some(8),
            json!({"signal": "useful", "section_signals": signals}),
        ));
        events.push(standalone(signals));
    }
    let mut malformed_standalone = standalone(json!({"overview": "useful"}));
    malformed_standalone.target_id = Some(Uuid::from_u128(9));
    malformed_standalone.payload["signal"] = json!("useful");
    events.push(malformed_standalone);
    for (index, event) in events.iter_mut().enumerate() {
        event.id = Uuid::from_u128(1_000 + index as u128);
        event.created_at = 1_000_000 + index as i64;
    }
    events
}

fn assert_balanced_equal(actual: &BalancedRecallState, expected: &BalancedRecallState) {
    assert_eq!(actual.to_snapshot(), expected.to_snapshot());
    assert_eq!(
        actual.entity_posteriors.capacity(),
        expected.entity_posteriors.capacity()
    );
}

fn section_snapshot(state: &SectionPosteriorState) -> Value {
    serde_json::to_value(state.to_snapshot()).unwrap()
}

#[test]
fn initial_state_empty_derive_and_public_section_constructors_match_legacy() {
    let context = FoldContext::new();
    for capacity in [0, 1, 2, 8] {
        let fold = BalancedRecallFold::new(capacity);
        let legacy = LegacyBalancedRecallFold::new(capacity);
        assert_balanced_equal(&fold.init(&context), &legacy.init(&context));
        let actual = fold.derive(std::iter::empty::<&Event>(), &context);
        let expected = legacy.derive(std::iter::empty::<&Event>(), &context);
        assert_balanced_equal(&actual.state, &expected.state);
        assert_eq!(actual.entries_processed, 0);
        assert_eq!(actual.entries_processed, expected.entries_processed);
    }
    let expected = LegacySectionPosteriorFold::new().init(&context);
    let constructors: [fn() -> SectionPosteriorFold; 2] =
        [SectionPosteriorFold::new, SectionPosteriorFold::default];
    for fold in
        std::iter::once(SectionPosteriorFold).chain(constructors.map(|construct| construct()))
    {
        assert_eq!(
            section_snapshot(&fold.init(&context)),
            section_snapshot(&expected)
        );
        let result = fold.derive(std::iter::empty::<&Event>(), &context);
        assert_eq!(section_snapshot(&result.state), section_snapshot(&expected));
        assert_eq!(result.entries_processed, 0);
    }
}

#[test]
fn mixed_sequence_matches_legacy_full_snapshots_after_every_prefix() {
    let context = FoldContext::new();
    let events = mixed_events();
    assert_eq!(events, mixed_events(), "fixture must be deterministic");
    for capacity in [0, 1, 2, 8] {
        let fold = BalancedRecallFold::new(capacity);
        let legacy = LegacyBalancedRecallFold::new(capacity);
        let section_fold = SectionPosteriorFold;
        let legacy_sections = LegacySectionPosteriorFold;
        let mut actual = fold.init(&context);
        let mut expected = legacy.init(&context);
        let mut actual_sections = section_fold.init(&context);
        let mut expected_sections = legacy_sections.init(&context);
        assert_balanced_equal(&actual, &expected);
        assert_eq!(
            section_snapshot(&actual_sections),
            section_snapshot(&expected_sections)
        );
        for event in &events {
            actual = fold.reduce(actual, event, &context);
            expected = legacy.reduce(expected, event, &context);
            actual_sections = section_fold.reduce(actual_sections, event, &context);
            expected_sections = legacy_sections.reduce(expected_sections, event, &context);
            assert_balanced_equal(&actual, &expected);
            assert_eq!(
                section_snapshot(&actual_sections),
                section_snapshot(&expected_sections)
            );
        }
        assert!(actual.total_events > 0);
        assert!(!actual.entity_posteriors.is_empty());
        assert!(actual_sections.total_events > 0);
        assert!(actual_sections.exploration_epoch < 50);
    }
}

#[test]
fn derive_matches_legacy_for_every_prefix_and_counts_filtered_entries() {
    let context = FoldContext::new();
    let events = mixed_events();
    let fold = BalancedRecallFold::new(2);
    let legacy = LegacyBalancedRecallFold::new(2);
    for end in 0..=events.len() {
        let actual = fold.derive(&events[..end], &context);
        let expected = legacy.derive(&events[..end], &context);
        assert_balanced_equal(&actual.state, &expected.state);
        assert_eq!(actual.entries_processed, end);
        assert_eq!(actual.entries_processed, expected.entries_processed);
        let actual_sections = SectionPosteriorFold.derive(&events[..end], &context);
        let expected_sections = LegacySectionPosteriorFold.derive(&events[..end], &context);
        assert_eq!(
            section_snapshot(&actual_sections.state),
            section_snapshot(&expected_sections.state)
        );
        assert_eq!(actual_sections.entries_processed, end);
        assert_eq!(
            actual_sections.entries_processed,
            expected_sections.entries_processed
        );
    }
    let dropped = events
        .iter()
        .filter(|e| e.kind == EventKind::FeedbackUnjudged)
        .count();
    assert!(dropped > 0);
    assert_eq!(
        fold.derive(&events, &context).state.total_events,
        (events.len() - dropped) as u64
    );
}

#[test]
fn supplied_state_capacity_priors_and_finalize_remain_unchanged() {
    let context = FoldContext::new();
    // A reduce call must use the supplied state's capacity, not reinitialize
    // from the constructor's deliberately different capacity.
    let fold = BalancedRecallFold::new(64);
    let legacy = LegacyBalancedRecallFold::new(64);
    let mut expected = BalancedRecallState::new(2);
    expected.relevance = BetaPosterior::new(11.0, 4.0);
    expected.total_events = 19;
    expected.exploration_epoch = 7;
    expected
        .entity_posteriors
        .get_or_insert(Uuid::from_u128(99), || BetaPosterior::new(3.0, 4.0));
    let mut actual = BalancedRecallState::from_snapshot(expected.to_snapshot(), 2);
    let mut expected_sections = SectionPosteriorState::from_priors(
        [(SectionType::Overview, BetaPosterior::new(9.0, 3.0))]
            .into_iter()
            .collect(),
    );
    expected_sections.total_events = 11;
    expected_sections.exploration_epoch = 2;
    let mut actual_sections = SectionPosteriorState::from_snapshot(expected_sections.to_snapshot());
    assert_balanced_equal(&actual, &expected);
    assert_eq!(
        section_snapshot(&actual_sections),
        section_snapshot(&expected_sections)
    );
    for event in mixed_events() {
        actual = fold.reduce(actual, &event, &context);
        expected = legacy.reduce(expected, &event, &context);
        actual_sections = SectionPosteriorFold.reduce(actual_sections, &event, &context);
        expected_sections = LegacySectionPosteriorFold.reduce(expected_sections, &event, &context);
        assert_balanced_equal(&actual, &expected);
        assert_eq!(actual.entity_posteriors.capacity(), 2);
        assert_eq!(
            section_snapshot(&actual_sections),
            section_snapshot(&expected_sections)
        );
    }
    let before = actual.to_snapshot();
    actual = fold.finalize(actual, &context);
    expected = legacy.finalize(expected, &context);
    assert_eq!(actual.to_snapshot(), before);
    assert_balanced_equal(&actual, &expected);
    let before_sections = section_snapshot(&actual_sections);
    actual_sections = SectionPosteriorFold.finalize(actual_sections, &context);
    expected_sections = LegacySectionPosteriorFold.finalize(expected_sections, &context);
    assert_eq!(section_snapshot(&actual_sections), before_sections);
    assert_eq!(
        section_snapshot(&actual_sections),
        section_snapshot(&expected_sections)
    );
}

#[test]
fn only_unjudged_kind_bypasses_balanced_event_count() {
    let context = FoldContext::new();
    let fold = BalancedRecallFold::new(2);
    let mut telemetry = event(
        "brain.feedback",
        Some(1),
        json!({"signal": "useful", "section_signals": {"overview": "useful"}}),
    );
    telemetry.kind = EventKind::FeedbackUnjudged;
    let initial = fold.init(&context);
    let before = initial.to_snapshot();
    let actual = fold.reduce(initial, &telemetry, &context);
    assert_eq!(actual.to_snapshot(), before);
    let events = [
        telemetry,
        event("unrelated", None, json!({})),
        event("brain.emit", Some(1), json!({"signal": "useful"})),
        event("brain.feedback", Some(1), json!({"signal": "invalid"})),
    ];
    let actual = fold.derive(&events, &context);
    assert_eq!(actual.entries_processed, 4);
    assert_eq!(actual.state.total_events, 3);
    assert!(actual.state.entity_posteriors.is_empty());
    assert_balanced_equal(
        &actual.state,
        &LegacyBalancedRecallFold::new(2)
            .derive(&events, &context)
            .state,
    );
}

#[test]
fn entity_eviction_order_matches_legacy_and_preserves_recency() {
    let context = FoldContext::new();
    let fold = BalancedRecallFold::new(2);
    let legacy = LegacyBalancedRecallFold::new(2);
    let mut actual = fold.init(&context);
    let mut expected = legacy.init(&context);
    for (id, order) in [
        (10, vec![10]),
        (11, vec![10, 11]),
        (12, vec![11, 12]),
        (11, vec![12, 11]),
        (13, vec![11, 13]),
    ] {
        let event = event("get", Some(id), json!({}));
        actual = fold.reduce(actual, &event, &context);
        expected = legacy.reduce(expected, &event, &context);
        assert_balanced_equal(&actual, &expected);
        assert_eq!(
            actual.entity_posteriors.order(),
            order.into_iter().map(Uuid::from_u128).collect::<Vec<_>>()
        );
    }
}

#[test]
fn standalone_section_validation_and_retired_entries_preserve_noops() {
    let context = FoldContext::new();
    let valid = standalone(json!({"overview": "useful", "references": "wrong"}));
    let initial = SectionPosteriorFold.init(&context);
    let initial_count = initial.total_events;
    let populated = SectionPosteriorFold.reduce(initial, &valid, &context);
    assert_eq!(populated.total_events, initial_count + 1);
    let before = section_snapshot(&populated);
    let mut invalid = vec![
        standalone(json!({})),
        standalone(json!(["overview"])),
        standalone(json!({"references": "useful", "other": "wrong"})),
        standalone(json!({"overview": "useful", "glossary": "wrong"})),
        standalone(json!({"overview": "invalid"})),
    ];
    let mut missing_profile = valid.clone();
    missing_profile
        .payload
        .as_object_mut()
        .unwrap()
        .remove("served_by_profile_id");
    invalid.push(missing_profile);
    let mut with_target = valid.clone();
    with_target.target_id = Some(Uuid::from_u128(1));
    with_target.payload["signal"] = json!("useful");
    invalid.push(with_target);
    let mut failed = valid.clone();
    failed.outcome = EventOutcome::Error;
    invalid.push(failed);
    let mut unjudged = valid.clone();
    unjudged.kind = EventKind::FeedbackUnjudged;
    invalid.push(unjudged);
    let mut bad_attribution = valid.clone();
    bad_attribution.payload["target_attribution"] = json!(42);
    invalid.push(bad_attribution);
    for event in invalid {
        let actual = SectionPosteriorFold.reduce(
            SectionPosteriorState::from_snapshot(populated.to_snapshot()),
            &event,
            &context,
        );
        let expected = LegacySectionPosteriorFold.reduce(
            SectionPosteriorState::from_snapshot(populated.to_snapshot()),
            &event,
            &context,
        );
        assert_eq!(section_snapshot(&actual), before);
        assert_eq!(section_snapshot(&actual), section_snapshot(&expected));
    }
}

#[test]
fn zero_effective_weight_preserves_posteriors_but_counts_balanced_input() {
    let context = FoldContext::new();
    let event = event(
        "brain.feedback",
        Some(1),
        json!({"signal": "correction", "gate": {"effective_weight": 0.0},
            "section_signals": {"overview": "wrong"}}),
    );
    let fold = BalancedRecallFold::new(2);
    let balanced = fold.init(&context);
    let mut expected = balanced.to_snapshot();
    expected.total_events += 1;
    let actual = fold.reduce(balanced, &event, &context);
    assert_eq!(actual.to_snapshot(), expected);
    let sections = SectionPosteriorFold.init(&context);
    let before = section_snapshot(&sections);
    let actual_sections = SectionPosteriorFold.reduce(sections, &event, &context);
    assert_eq!(section_snapshot(&actual_sections), before);
}
