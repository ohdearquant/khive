use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_storage::event::Event;
use khive_types::EventOutcome;

/// Feedback signal values for the `brain.feedback` verb (ADR-032 §3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSignal {
    Useful,
    NotUseful,
    Wrong,
}

/// Interpreted brain signal extracted from a raw Event (ADR-032 §4).
///
/// `interpret()` is the single mapping layer from the shared event log to
/// brain-internal signals. No parallel event enum is needed; the Event
/// substrate IS the source of truth.
#[derive(Debug)]
pub enum BrainSignal {
    /// A recall verb succeeded — positive signal for the recalled entity.
    RecallHit { target_id: Uuid, latency_us: i64 },
    /// A recall verb returned no results — miss signal for tuning.
    RecallMiss,
    /// A search verb completed.
    SearchCompleted { latency_us: i64 },
    /// Explicit feedback on a specific entity, emitted by `brain.feedback`.
    Feedback {
        target_id: Uuid,
        signal: FeedbackSignal,
        /// Profile that served the event being rated, if known.
        served_by_profile_id: Option<String>,
    },
    /// Any other note-substrate access (get, list on notes).
    NoteAccessed { target_id: Uuid },
    /// Event is not relevant to the brain.
    Irrelevant,
}

/// Extract a brain signal from a raw storage Event (ADR-032 §4).
///
/// `brain.emit` is no longer handled here — it was renamed to `brain.feedback`
/// per ADR-032 §11 (`brain.feedback` is the `FeedbackExplicit` event emitter).
/// Any `brain.emit` event that predates this ADR is treated as Irrelevant so
/// that old event log entries do not cause spurious feedback updates.
///
/// To add a new signal source: add one match arm to this function. That is
/// the entire extension surface (ADR-032 §4).
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
        // brain.feedback is the ADR-032 §11 verb for FeedbackExplicit events.
        // (brain.emit predates this ADR; treated as Irrelevant for old replays.)
        "brain.feedback" => {
            let target = match event.target_id {
                Some(t) => t,
                None => return BrainSignal::Irrelevant,
            };
            let signal = event
                .payload
                .get("signal")
                .and_then(|s| serde_json::from_value::<FeedbackSignal>(s.clone()).ok());
            let served_by = event
                .payload
                .get("served_by_profile_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
            match signal {
                Some(s) => BrainSignal::Feedback {
                    target_id: target,
                    signal: s,
                    served_by_profile_id: served_by,
                },
                None => BrainSignal::Irrelevant,
            }
        }
        "get" | "remember" => match event.target_id {
            Some(tid) => BrainSignal::NoteAccessed { target_id: tid },
            None => BrainSignal::Irrelevant,
        },
        _ => BrainSignal::Irrelevant,
    }
}

/// Extract (entity_id, positive_signal) for per-entity posterior updates.
pub fn entity_signal(signal: &BrainSignal) -> Option<(Uuid, bool)> {
    match signal {
        BrainSignal::RecallHit { target_id, .. } => Some((*target_id, true)),
        BrainSignal::NoteAccessed { target_id } => Some((*target_id, true)),
        BrainSignal::Feedback {
            target_id, signal, ..
        } => Some((*target_id, matches!(signal, FeedbackSignal::Useful))),
        BrainSignal::RecallMiss | BrainSignal::SearchCompleted { .. } | BrainSignal::Irrelevant => {
            None
        }
    }
}

/// Is this signal positive for the global recall parameter?
pub fn is_recall_positive(signal: &BrainSignal) -> Option<bool> {
    match signal {
        BrainSignal::RecallHit { .. } => Some(true),
        BrainSignal::RecallMiss => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::{EventKind, SubstrateKind};

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
        // brain.emit predates ADR-032; old log entries must not trigger feedback.
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
}
