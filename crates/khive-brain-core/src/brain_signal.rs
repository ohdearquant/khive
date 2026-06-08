//! Brain signal types — the decoded signal vocabulary for profile state updates.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{FeedbackEventKind, FeedbackSignal, SectionType};

/// Interpreted brain signal for profile state updates.
///
/// Produced by `interpret(event)` in pack-brain. Consumed by
/// `BalancedRecallState::apply_signal` and `SectionPosteriorState::apply_signal`.
#[derive(Debug)]
pub enum BrainSignal {
    RecallHit {
        target_id: Uuid,
        latency_us: i64,
    },
    RecallMiss,
    SearchCompleted {
        latency_us: i64,
    },
    Feedback {
        target_id: Uuid,
        signal: FeedbackSignal,
        // Retained for replay/backtest completeness per ADR-032 §3.
        #[allow(dead_code)]
        served_by_profile_id: Option<String>,
        section_signals: Option<HashMap<SectionType, FeedbackSignal>>,
    },
    SemanticFeedback {
        target_id: Uuid,
        event_kind: FeedbackEventKind,
        // Retained for replay/backtest completeness per ADR-032 §3.
        #[allow(dead_code)]
        served_by_profile_id: Option<String>,
    },
    NoteAccessed {
        target_id: Uuid,
    },
    Irrelevant,
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
