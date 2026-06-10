//! Brain event interpretation — maps raw `Event` records to typed `BrainSignal` values.

use std::collections::HashMap;

use khive_storage::event::Event;
use khive_types::EventOutcome;

pub use khive_brain_core::BrainSignal;
use khive_brain_core::{FeedbackEventKind, FeedbackSignal, SectionType};

/// Extract a brain signal from a raw storage Event.
///
/// `brain.emit` is no longer handled here — it was renamed to `brain.feedback`
/// (`brain.feedback` is the `FeedbackExplicit` event emitter).
/// Any `brain.emit` event that predates this rename is treated as Irrelevant so
/// that old event log entries do not cause spurious feedback updates.
///
/// To add a new signal source: add one match arm to this function.
pub fn interpret(event: &Event) -> BrainSignal {
    match event.verb.as_str() {
        "recall" => match event.outcome {
            EventOutcome::Success => match event.target_id {
                Some(tid) => BrainSignal::RecallHit {
                    target_id: tid,
                    latency_us: event.duration_us,
                },
                None => BrainSignal::RecallMiss,
            },
            _ => BrainSignal::RecallMiss,
        },
        "search" => BrainSignal::SearchCompleted {
            latency_us: event.duration_us,
        },
        // brain.feedback is the verb for FeedbackExplicit events.
        // (brain.emit predates this rename; treated as Irrelevant for old replays.)
        "brain.feedback" => {
            let target = match event.target_id {
                Some(t) => t,
                None => return BrainSignal::Irrelevant,
            };
            let signal_str = event
                .payload
                .get("signal")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let served_by = event
                .payload
                .get("served_by_profile_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
            // Parse section_signals through the shared validator so that semantically
            // poisoned entries (empty map, unknown section, out-of-contract signal value)
            // produce None — identical treatment during both live handler and replay.
            let section_signals = event.payload.get("section_signals").and_then(|v| {
                // Reject anything the shared validator would have rejected up front.
                // This is the replay path: invalid entries yield None (→ Irrelevant for
                // the section fold), and the caller (persist.rs) quarantines the whole
                // event before calling apply_signal.
                if crate::validate_section_signals(v).is_err() {
                    return None;
                }
                serde_json::from_value::<HashMap<SectionType, FeedbackSignal>>(v.clone()).ok()
            });

            // Issue #268: try semantic event kind names first, then fall back to
            // legacy FeedbackSignal (useful / not_useful / wrong).
            if let Some(event_kind) = FeedbackEventKind::from_signal_str(signal_str) {
                BrainSignal::SemanticFeedback {
                    target_id: target,
                    event_kind,
                    served_by_profile_id: served_by,
                }
            } else {
                let signal = serde_json::from_value::<FeedbackSignal>(serde_json::Value::String(
                    signal_str.to_owned(),
                ))
                .ok();
                match signal {
                    Some(s) => BrainSignal::Feedback {
                        target_id: target,
                        signal: s,
                        served_by_profile_id: served_by,
                        section_signals,
                    },
                    None => BrainSignal::Irrelevant,
                }
            }
        }
        "get" | "remember" => match event.target_id {
            Some(tid) => BrainSignal::NoteAccessed { target_id: tid },
            None => BrainSignal::Irrelevant,
        },
        _ => BrainSignal::Irrelevant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_brain_core::{entity_signal, is_recall_positive};
    use khive_types::{EventKind, SubstrateKind};
    use uuid::Uuid;

    fn make_event(verb: &str, outcome: EventOutcome, target: Option<Uuid>) -> Event {
        let mut e = Event::new("test", verb, EventKind::Audit, SubstrateKind::Note, "brain");
        e.outcome = outcome;
        e.target_id = target;
        e
    }

    #[test]
    fn recall_success_with_target_is_hit() {
        let id = Uuid::new_v4();
        let e = make_event("recall", EventOutcome::Success, Some(id));
        match interpret(&e) {
            BrainSignal::RecallHit { target_id, .. } => assert_eq!(target_id, id),
            other => panic!("expected RecallHit, got {other:?}"),
        }
    }

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

    #[test]
    fn entity_signal_for_hit() {
        let id = Uuid::new_v4();
        let sig = BrainSignal::RecallHit {
            target_id: id,
            latency_us: 100,
        };
        assert_eq!(entity_signal(&sig), Some((id, true)));
    }

    #[test]
    fn entity_signal_for_miss() {
        assert_eq!(entity_signal(&BrainSignal::RecallMiss), None);
    }

    #[test]
    fn recall_positive_classification() {
        let hit = BrainSignal::RecallHit {
            target_id: Uuid::new_v4(),
            latency_us: 0,
        };
        assert_eq!(is_recall_positive(&hit), Some(true));
        assert_eq!(is_recall_positive(&BrainSignal::RecallMiss), Some(false));
        assert_eq!(
            is_recall_positive(&BrainSignal::SearchCompleted { latency_us: 0 }),
            None
        );
    }

    #[test]
    fn feedback_not_useful_is_negative_entity_signal() {
        let id = Uuid::new_v4();
        let sig = BrainSignal::Feedback {
            target_id: id,
            signal: FeedbackSignal::NotUseful,
            served_by_profile_id: None,
            section_signals: None,
        };
        assert_eq!(entity_signal(&sig), Some((id, false)));
    }

    #[test]
    fn feedback_wrong_is_negative_entity_signal() {
        let id = Uuid::new_v4();
        let sig = BrainSignal::Feedback {
            target_id: id,
            signal: FeedbackSignal::Wrong,
            served_by_profile_id: None,
            section_signals: None,
        };
        assert_eq!(entity_signal(&sig), Some((id, false)));
    }

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

    // ── FeedbackEventKind unit tests (MAJ-001 coverage) ──────────────────────

    #[test]
    fn feedback_event_kind_from_signal_str_all_variants() {
        assert_eq!(
            FeedbackEventKind::from_signal_str("explicit_positive"),
            Some(FeedbackEventKind::ExplicitPositive)
        );
        assert_eq!(
            FeedbackEventKind::from_signal_str("explicit_negative"),
            Some(FeedbackEventKind::ExplicitNegative)
        );
        assert_eq!(
            FeedbackEventKind::from_signal_str("implicit_positive"),
            Some(FeedbackEventKind::ImplicitPositive)
        );
        assert_eq!(
            FeedbackEventKind::from_signal_str("implicit_negative"),
            Some(FeedbackEventKind::ImplicitNegative)
        );
        assert_eq!(
            FeedbackEventKind::from_signal_str("correction"),
            Some(FeedbackEventKind::Correction)
        );
    }

    #[test]
    fn feedback_event_kind_from_signal_str_unknown_returns_none() {
        assert_eq!(FeedbackEventKind::from_signal_str("useful"), None);
        assert_eq!(FeedbackEventKind::from_signal_str("not_useful"), None);
        assert_eq!(FeedbackEventKind::from_signal_str(""), None);
        assert_eq!(FeedbackEventKind::from_signal_str("ExplicitPositive"), None);
    }

    #[test]
    fn feedback_event_kind_update_weight_values() {
        assert!((FeedbackEventKind::Correction.update_weight() - 2.0).abs() < 1e-12);
        assert!((FeedbackEventKind::ExplicitPositive.update_weight() - 1.5).abs() < 1e-12);
        assert!((FeedbackEventKind::ExplicitNegative.update_weight() - 1.5).abs() < 1e-12);
        assert!((FeedbackEventKind::ImplicitPositive.update_weight() - 0.5).abs() < 1e-12);
        assert!((FeedbackEventKind::ImplicitNegative.update_weight() - 0.5).abs() < 1e-12);
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
            } => {
                assert_eq!(target_id, id);
                assert_eq!(event_kind, FeedbackEventKind::ExplicitPositive);
                assert!(served_by_profile_id.is_none());
            }
            other => panic!("expected SemanticFeedback, got {other:?}"),
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
        };
        assert_eq!(entity_signal(&sig), Some((id, false)));
    }
}
