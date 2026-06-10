//! Brain signal types — the decoded signal vocabulary for profile state updates.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{FeedbackEventKind, FeedbackSignal, SectionType};

/// Interpreted brain signal for profile state updates.
///
/// Produced by `interpret(event)` in pack-brain. Consumed by
/// `BalancedRecallState::apply_signal` and `SectionPosteriorState::apply_signal`.
#[derive(Debug, Clone)]
pub enum BrainSignal {
    /// A memory or entity was returned and confirmed relevant by the caller.
    RecallHit { target_id: Uuid, latency_us: i64 },
    /// A recall query returned no results for the given criteria.
    RecallMiss,
    /// A hybrid search operation completed; latency is recorded but does not
    /// currently update any brain posterior (only `RecallHit`/`RecallMiss` do).
    SearchCompleted { latency_us: i64 },
    /// An explicit feedback signal from the caller for a specific record.
    Feedback {
        target_id: Uuid,
        signal: FeedbackSignal,
        // Retained for replay/backtest completeness per ADR-032 §3.
        #[allow(dead_code)]
        served_by_profile_id: Option<String>,
        section_signals: Option<HashMap<SectionType, FeedbackSignal>>,
    },
    /// An implicit semantic feedback signal derived from user interaction patterns.
    SemanticFeedback {
        target_id: Uuid,
        event_kind: FeedbackEventKind,
        // Retained for replay/backtest completeness per ADR-032 §3.
        #[allow(dead_code)]
        served_by_profile_id: Option<String>,
    },
    /// A note record was accessed; counts as a positive signal for its entity.
    NoteAccessed { target_id: Uuid },
    /// Event did not produce a useful signal and should be discarded.
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
