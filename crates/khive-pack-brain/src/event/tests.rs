use super::*;
use khive_brain_core::{entity_signal, is_recall_positive, SectionPosteriorState};
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

fn make_event(verb: &str, outcome: EventOutcome, target: Option<Uuid>) -> Event {
    let mut e = Event::new("test", verb, EventKind::Audit, SubstrateKind::Note, "brain");
    e.outcome = outcome;
    e.target_id = target;
    e
}

#[test]
fn unjudged_kind_precedes_signal_and_verb_interpretation() {
    for verb in ["brain.feedback", "recall", "search", "get"] {
        let mut event = make_event(verb, EventOutcome::Success, Some(Uuid::new_v4()));
        event.kind = EventKind::FeedbackUnjudged;
        event.payload = serde_json::json!({
            "signal": "useful",
            "section_signals": {"overview": "useful"},
            "served_by_profile_id": "telemetry-only-profile"
        });
        assert!(matches!(interpret(&event), BrainSignal::Irrelevant));
    }
}

include!("tests/recall_shapes.rs");

#[test]
fn recall_success_without_target_is_miss() {
    let e = make_event("recall", EventOutcome::Success, None);
    assert!(matches!(interpret(&e), BrainSignal::RecallMiss));
}

#[test]
fn recall_error_is_miss() {
    let e = make_event("recall", EventOutcome::Error, Some(Uuid::new_v4()));
    assert!(matches!(interpret(&e), BrainSignal::RecallMiss));
}

#[test]
fn search_is_completed() {
    let e = make_event("search", EventOutcome::Success, None);
    assert!(matches!(interpret(&e), BrainSignal::SearchCompleted { .. }));
}

#[test]
fn brain_feedback_with_useful_signal() {
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"signal": "useful"});
    match interpret(&e) {
        BrainSignal::Feedback {
            target_id,
            signal,
            served_by_profile_id,
            ..
        } => {
            assert_eq!(target_id, id);
            assert_eq!(signal, FeedbackSignal::Useful);
            assert!(served_by_profile_id.is_none());
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn brain_feedback_with_served_by_profile_id() {
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({
        "signal": "not_useful",
        "served_by_profile_id": "balanced-recall-v1"
    });
    match interpret(&e) {
        BrainSignal::Feedback {
            target_id,
            signal,
            served_by_profile_id,
            ..
        } => {
            assert_eq!(target_id, id);
            assert_eq!(signal, FeedbackSignal::NotUseful);
            assert_eq!(served_by_profile_id.as_deref(), Some("balanced-recall-v1"));
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn brain_feedback_with_resolution_marker_decodes_served_by() {
    // #1016: handle_feedback now always stamps the resolved profile plus a
    // `profile_resolution` marker. The marker is payload-only metadata —
    // the decoder must surface served_by_profile_id and ignore the marker.
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({
        "signal": "useful",
        "served_by_profile_id": "balanced-recall-v1",
        "profile_resolution": "default"
    });
    match interpret(&e) {
        BrainSignal::Feedback {
            target_id,
            served_by_profile_id,
            ..
        } => {
            assert_eq!(target_id, id);
            assert_eq!(served_by_profile_id.as_deref(), Some("balanced-recall-v1"));
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn brain_feedback_without_target_is_irrelevant() {
    let e = make_event("brain.feedback", EventOutcome::Success, None);
    assert!(matches!(interpret(&e), BrainSignal::Irrelevant));
}

#[test]
fn brain_emit_legacy_is_irrelevant() {
    // brain.emit predates brain.feedback; old log entries must not trigger feedback.
    let id = Uuid::new_v4();
    let mut e = make_event("brain.emit", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"signal": "useful"});
    assert!(matches!(interpret(&e), BrainSignal::Irrelevant));
}

#[test]
fn unknown_verb_is_irrelevant() {
    let e = make_event("link", EventOutcome::Success, Some(Uuid::new_v4()));
    assert!(matches!(interpret(&e), BrainSignal::Irrelevant));
}

include!("tests/entity_signal_values.rs");

#[test]
fn brain_feedback_invalid_signal_data_is_irrelevant() {
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"signal": "bad_value"});
    assert!(matches!(interpret(&e), BrainSignal::Irrelevant));
}

#[test]
fn note_accessed_via_get_verb_is_positive_entity_signal() {
    let id = Uuid::new_v4();
    let e = make_event("get", EventOutcome::Success, Some(id));
    match interpret(&e) {
        BrainSignal::NoteAccessed { target_id } => {
            assert_eq!(target_id, id);
            assert_eq!(
                entity_signal(&BrainSignal::NoteAccessed { target_id }),
                Some((id, true))
            );
        }
        other => panic!("expected NoteAccessed, got {other:?}"),
    }
}

#[test]
fn note_accessed_via_remember_verb_is_positive_entity_signal() {
    let id = Uuid::new_v4();
    let e = make_event("remember", EventOutcome::Success, Some(id));
    match interpret(&e) {
        BrainSignal::NoteAccessed { target_id } => {
            assert_eq!(target_id, id);
        }
        other => panic!("expected NoteAccessed, got {other:?}"),
    }
}

#[test]
fn feedback_with_section_signals() {
    use khive_brain_core::SectionType;
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({
        "signal": "useful",
        "section_signals": {
            "overview": "useful",
            "formalism": "not_useful",
            "examples": "wrong"
        }
    });
    match interpret(&e) {
        BrainSignal::Feedback {
            section_signals, ..
        } => {
            let ss = section_signals.expect("section_signals should be parsed");
            assert_eq!(ss.len(), 3);
            assert_eq!(ss[&SectionType::Overview], FeedbackSignal::Useful);
            assert_eq!(ss[&SectionType::Formalism], FeedbackSignal::NotUseful);
            assert_eq!(ss[&SectionType::Examples], FeedbackSignal::Wrong);
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn semantic_feedback_preserves_section_signals_for_replay() {
    use khive_brain_core::SectionType;
    let id = Uuid::new_v4();
    let mut event = make_event("brain.feedback", EventOutcome::Success, Some(id));
    event.payload = serde_json::json!({
        "signal": "explicit_positive",
        "section_signals": {"overview": "useful", "examples": "wrong"}
    });
    match interpret(&event) {
        BrainSignal::SemanticFeedback {
            section_signals: Some(signals),
            ..
        } => {
            assert_eq!(signals[&SectionType::Overview], FeedbackSignal::Useful);
            assert_eq!(signals[&SectionType::Examples], FeedbackSignal::Wrong);
        }
        other => panic!("semantic section signals were dropped: {other:?}"),
    }
}

// ── Feedback kinds retain their update weights and polarity ─────────────

include!("tests/feedback_kind_parsing.rs");

#[test]
fn feedback_event_kind_update_weight_values() {
    assert!((FeedbackEventKind::Correction.update_weight() - 2.0).abs() < 1e-12);
    assert!((FeedbackEventKind::ExplicitPositive.update_weight() - 1.5).abs() < 1e-12);
    assert!((FeedbackEventKind::ExplicitNegative.update_weight() - 1.5).abs() < 1e-12);
    assert!((FeedbackEventKind::ImplicitPositive.update_weight() - 0.1).abs() < 1e-12);
    assert!((FeedbackEventKind::ImplicitNegative.update_weight() - 0.1).abs() < 1e-12);
}

#[test]
fn feedback_event_kind_is_positive_classification() {
    assert!(FeedbackEventKind::ExplicitPositive.is_positive());
    assert!(FeedbackEventKind::ImplicitPositive.is_positive());
    assert!(!FeedbackEventKind::ExplicitNegative.is_positive());
    assert!(!FeedbackEventKind::ImplicitNegative.is_positive());
    assert!(!FeedbackEventKind::Correction.is_positive());
}

#[test]
fn brain_feedback_semantic_explicit_positive_produces_semantic_signal() {
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"signal": "explicit_positive"});
    match interpret(&e) {
        BrainSignal::SemanticFeedback {
            target_id,
            event_kind,
            served_by_profile_id,
            section_signals,
            effective_weight,
        } => {
            assert_eq!(target_id, id);
            assert_eq!(event_kind, FeedbackEventKind::ExplicitPositive);
            assert!(served_by_profile_id.is_none());
            assert!(section_signals.is_none());
            assert!((effective_weight - 1.5).abs() < 1e-12);
        }
        other => panic!("expected SemanticFeedback, got {other:?}"),
    }
}

#[test]
fn semantic_feedback_keeps_section_signals_for_every_scalar() {
    for scalar in [
        "explicit_positive",
        "explicit_negative",
        "implicit_positive",
        "implicit_negative",
        "correction",
    ] {
        let mut event = make_event(
            "brain.feedback",
            EventOutcome::Success,
            Some(Uuid::new_v4()),
        );
        event.payload = serde_json::json!({
            "signal": scalar,
            "section_signals": {
                "overview": "useful",
                "formalism": "not_useful",
                "examples": "wrong"
            }
        });

        let signal = interpret(&event);
        let BrainSignal::SemanticFeedback {
            section_signals: Some(sections),
            ..
        } = &signal
        else {
            panic!("{scalar} must retain section signals: {signal:?}");
        };
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[&SectionType::Overview], FeedbackSignal::Useful);
        assert_eq!(sections[&SectionType::Formalism], FeedbackSignal::NotUseful);
        assert_eq!(sections[&SectionType::Examples], FeedbackSignal::Wrong);

        let mut state = SectionPosteriorState::new();
        state.apply_signal(&signal);
        assert_eq!(state.total_events, 1, "{scalar}");
        let weight = FeedbackEventKind::from_signal_str(scalar)
            .expect("known semantic scalar")
            .update_weight();
        assert!((state.posteriors[&SectionType::Overview].alpha() - 2.0 - weight).abs() < 1e-12);
        assert!((state.posteriors[&SectionType::Formalism].beta() - 4.0 - weight).abs() < 1e-12);
        assert!(
            (state.posteriors[&SectionType::Examples].beta() - 2.0 - 2.0 * weight).abs() < 1e-12
        );
    }
}

#[test]
fn feedback_without_section_signals_is_none() {
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"signal": "useful"});
    match interpret(&e) {
        BrainSignal::Feedback {
            section_signals, ..
        } => {
            assert!(section_signals.is_none());
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn brain_feedback_semantic_correction_produces_semantic_signal() {
    let id = Uuid::new_v4();
    let mut e = make_event("brain.feedback", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"signal": "correction"});
    match interpret(&e) {
        BrainSignal::SemanticFeedback {
            target_id,
            event_kind,
            ..
        } => {
            assert_eq!(target_id, id);
            assert_eq!(event_kind, FeedbackEventKind::Correction);
        }
        other => panic!("expected SemanticFeedback, got {other:?}"),
    }
}

#[test]
fn semantic_feedback_entity_signal_positive_for_explicit_positive() {
    let id = Uuid::new_v4();
    let sig = BrainSignal::SemanticFeedback {
        target_id: id,
        event_kind: FeedbackEventKind::ExplicitPositive,
        served_by_profile_id: None,
        section_signals: None,
        effective_weight: FeedbackEventKind::ExplicitPositive.update_weight(),
    };
    assert_eq!(entity_signal(&sig), Some((id, true)));
}

#[test]
fn semantic_feedback_entity_signal_negative_for_implicit_negative() {
    let id = Uuid::new_v4();
    let sig = BrainSignal::SemanticFeedback {
        target_id: id,
        event_kind: FeedbackEventKind::ImplicitNegative,
        served_by_profile_id: None,
        section_signals: None,
        effective_weight: FeedbackEventKind::ImplicitNegative.update_weight(),
    };
    assert_eq!(entity_signal(&sig), Some((id, false)));
}

#[test]
fn semantic_feedback_entity_signal_negative_for_correction() {
    let id = Uuid::new_v4();
    let sig = BrainSignal::SemanticFeedback {
        target_id: id,
        event_kind: FeedbackEventKind::Correction,
        served_by_profile_id: None,
        section_signals: None,
        effective_weight: FeedbackEventKind::Correction.update_weight(),
    };
    assert_eq!(entity_signal(&sig), Some((id, false)));
}
