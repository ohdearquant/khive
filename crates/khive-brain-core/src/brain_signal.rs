//! Brain signal types — the decoded signal vocabulary for profile state updates.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{FeedbackEventKind, FeedbackSignal, SectionType};

/// Serve-time profile attribution carried across recall and feedback.
///
/// This is deliberately a three-state wire contract. `Unspecified` means the
/// caller supplied no serve-time evidence, so legacy binding/default resolution
/// remains permitted. `Unattributed` is explicit negative knowledge: a profile
/// was selected but its record could not be read, so no profile actually served
/// and downstream consumers must not guess one. `Profile` accompanies a
/// `served_by_profile_id` that was successfully read and used for serving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeAttribution {
    Profile,
    Unattributed,
    #[default]
    Unspecified,
}

impl ServeAttribution {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Unattributed => "unattributed",
            Self::Unspecified => "unspecified",
        }
    }

    /// Decode a recall event payload while preserving pre-tri-state events.
    ///
    /// A legacy event with only `served_by_profile_id` is attributed. Invalid
    /// or contradictory new payloads fail closed as `Unattributed` rather than
    /// crediting the default profile.
    pub fn from_wire(marker: Option<&str>, served_by_profile_id: Option<&str>) -> Self {
        match (marker, served_by_profile_id) {
            (Some("profile"), Some(_)) | (None, Some(_)) => Self::Profile,
            (Some("unspecified"), None) | (None, None) => Self::Unspecified,
            (Some("unattributed"), None) => Self::Unattributed,
            _ => Self::Unattributed,
        }
    }
}

/// Interpreted brain signal for profile state updates.
///
/// Produced by `interpret(event)` in pack-brain. Consumed by
/// `BalancedRecallState::apply_signal` and `SectionPosteriorState::apply_signal`.
#[derive(Debug, Clone)]
pub enum BrainSignal {
    /// A memory or entity was returned and confirmed relevant by the caller.
    RecallHit {
        target_id: Uuid,
        latency_us: i64,
        served_by_profile_id: Option<String>,
        serve_attribution: ServeAttribution,
    },
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
        /// The weight actually folded into posteriors for this event.
        ///
        /// Normally `event_kind.update_weight()`, but the ADR-081 §2 fold gate can
        /// clamp an implicit event to `0.0` when the decayed per-key implicit mass
        /// would exceed the cap. The decision is made once, at fold time, and
        /// persisted on the event payload (`interpret` reads it back) so that
        /// replay reproduces the exact same posterior update deterministically
        /// rather than re-evaluating the gate against current (already-mutated)
        /// mass-table state.
        effective_weight: f64,
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
