//! Brain event interpretation — maps raw `Event` records to typed `BrainSignal` values.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_storage::event::Event;
use khive_types::EventOutcome;

use khive_brain_core::SectionType;

/// Feedback signal values for the `brain.feedback` verb.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSignal {
    Useful,
    NotUseful,
    Wrong,
}

/// Semantic event taxonomy for brain fold updates (issue #268).
///
/// Captures the *kind* of feedback event so that the fold can apply
/// different update magnitudes to posteriors. Explicit signals carry
/// stronger evidence than implicit ones; corrections are strongest of all.
///
/// Update magnitude guidelines (applied by `FeedbackEventKind::update_weight`):
///   - `Correction`        → 2.0× (strongest — user actively corrected output)
///   - `ExplicitPositive`  → 1.5× (user explicitly marked as good)
///   - `ExplicitNegative`  → 1.5× (user explicitly marked as bad)
///   - `ImplicitPositive`  → 0.5× (user expanded / interacted — weaker signal)
///   - `ImplicitNegative`  → 0.5× (user skipped / ignored — weaker signal)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackEventKind {
    /// User explicitly rated a result as good (e.g., clicked "thumbs up").
    ExplicitPositive,
    /// User explicitly rated a result as bad (e.g., clicked "thumbs down").
    ExplicitNegative,
    /// User implicitly signalled satisfaction (e.g., expanded a section, dwell time).
    ImplicitPositive,
    /// User implicitly signalled dissatisfaction (e.g., skipped result, quick dismiss).
    ImplicitNegative,
    /// User corrected the output — strongest signal; overrides relevance posterior.
    Correction,
}

impl FeedbackEventKind {
    /// Magnitude multiplier for posterior updates.
    ///
    /// The fold multiplies the standard Beta update step (+1 to α or β) by this
    /// weight to produce fractional updates. Explicit evidence counts more than
    /// implicit; corrections count most (2×).
    pub fn update_weight(&self) -> f64 {
        match self {
            FeedbackEventKind::Correction => 2.0,
            FeedbackEventKind::ExplicitPositive | FeedbackEventKind::ExplicitNegative => 1.5,
            FeedbackEventKind::ImplicitPositive | FeedbackEventKind::ImplicitNegative => 0.5,
        }
    }

    /// Whether this event kind represents a positive signal.
    pub fn is_positive(&self) -> bool {
        matches!(
            self,
            FeedbackEventKind::ExplicitPositive | FeedbackEventKind::ImplicitPositive
        )
    }

    /// Parse from the `signal` string in a `brain.feedback` event payload.
    ///
    /// Accepts the semantic event kind names. Falls back to `None` when the
    /// string is not a recognised `FeedbackEventKind` (callers can then try
    /// parsing as the legacy `FeedbackSignal` enum).
    pub fn from_signal_str(s: &str) -> Option<Self> {
        match s {
            "explicit_positive" => Some(FeedbackEventKind::ExplicitPositive),
            "explicit_negative" => Some(FeedbackEventKind::ExplicitNegative),
            "implicit_positive" => Some(FeedbackEventKind::ImplicitPositive),
            "implicit_negative" => Some(FeedbackEventKind::ImplicitNegative),
            "correction" => Some(FeedbackEventKind::Correction),
            _ => None,
        }
    }
}

/// Interpreted brain signal extracted from a raw Event.
///
/// `interpret()` is the single mapping layer from the shared event log to
/// brain-internal signals. No parallel event enum is needed; the Event
/// substrate is the source of truth.
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
        section_signals: Option<HashMap<SectionType, FeedbackSignal>>,
    },
    /// Semantic feedback with event kind (issue #268).
    ///
    /// Produced when the `signal` field in a `brain.feedback` event is one of
    /// the `FeedbackEventKind` names (`explicit_positive`, `correction`, etc.).
    /// The fold uses `event_kind.update_weight()` to scale the posterior update.
    SemanticFeedback {
        target_id: Uuid,
        event_kind: FeedbackEventKind,
        served_by_profile_id: Option<String>,
    },
    /// Any other note-substrate access (get, list on notes).
    NoteAccessed { target_id: Uuid },
    /// Event is not relevant to the brain.
    Irrelevant,
}

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
            let section_signals = event.payload.get("section_signals").and_then(|v| {
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

/// Extract (entity_id, positive_signal) for per-entity posterior updates.
pub fn entity_signal(signal: &BrainSignal) -> Option<(Uuid, bool)> {
    match signal {
        BrainSignal::RecallHit { target_id, .. } => Some((*target_id, true)),
        BrainSignal::NoteAccessed { target_id } => Some((*target_id, true)),
        BrainSignal::Feedback {
            target_id, signal, ..
        } => Some((*target_id, matches!(signal, FeedbackSignal::Useful))),
        BrainSignal::SemanticFeedback {
            target_id,
            event_kind,
            ..
        } => Some((*target_id, event_kind.is_positive())),
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
