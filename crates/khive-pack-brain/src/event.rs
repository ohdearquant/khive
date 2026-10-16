//! Brain event interpretation — maps raw `Event` records to typed `BrainSignal` values.

use std::collections::HashMap;

use khive_storage::event::Event;
use khive_types::{EventKind, EventOutcome};

pub use khive_brain_core::BrainSignal;
use khive_brain_core::{FeedbackEventKind, FeedbackSignal, SectionType, ServeAttribution};

/// Extract a brain signal from a raw storage Event.
///
/// `brain.emit` is no longer handled here — it was renamed to `brain.feedback`
/// (`brain.feedback` is the `FeedbackExplicit` event emitter).
/// Any `brain.emit` event that predates this rename is treated as Irrelevant so
/// that old event log entries do not cause spurious feedback updates.
///
/// The runtime dispatch hook (`khive-runtime`) emits the namespaced verb
/// (e.g. `"memory.recall"`) as the verb it actually dispatched. `"recall"` is
/// retained as a legacy alias for event-log rows predating namespacing.
///
/// To add a new signal source: add one match arm to this function.
pub fn interpret(event: &Event) -> BrainSignal {
    // Kind is authoritative even if telemetry carries judgment-looking fields.
    if event.kind == EventKind::FeedbackUnjudged {
        return BrainSignal::Irrelevant;
    }

    match event.verb.as_str() {
        "memory.recall" | "recall" => match event.outcome {
            EventOutcome::Success => match event.target_id {
                Some(tid) => {
                    let served_by_profile_id = event
                        .payload
                        .get("served_by_profile_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let serve_attribution = ServeAttribution::from_wire(
                        event
                            .payload
                            .get("serve_attribution")
                            .and_then(|v| v.as_str()),
                        served_by_profile_id.as_deref(),
                    );
                    BrainSignal::RecallHit {
                        target_id: tid,
                        latency_us: event.duration_us,
                        served_by_profile_id,
                        serve_attribution,
                    }
                }
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
            let section_signals = event
                .payload
                .get("section_signals")
                .and_then(crate::replay_section_signals)
                .and_then(|v| {
                    // Reject anything the shared validator would have rejected up front.
                    // This is the replay path: invalid entries yield None (→ Irrelevant for
                    // the section fold), and the caller (persist.rs) quarantines the whole
                    // event before calling apply_signal. Entries keyed by a retired section
                    // type were dropped above, so the remaining signals still apply.
                    if crate::validate_section_signals(&v).is_err() {
                        return None;
                    }
                    serde_json::from_value::<HashMap<SectionType, FeedbackSignal>>(v).ok()
                });

            // Try semantic event kind names first, then fall back to
            // legacy FeedbackSignal (useful / not_useful / wrong).
            if let Some(event_kind) = FeedbackEventKind::from_signal_str(signal_str) {
                // ADR-081 §2: the fold gate (handlers::handle_feedback) computes the
                // actually-applied weight once, at emit time, and stamps it onto the
                // event payload under "gate.effective_weight" before persisting. Replay
                // reads the same stamp so the posterior update is reproduced exactly,
                // rather than re-evaluating the gate against present-day mass state.
                // Events without the stamp (pre-ADR-081 history, or non-gated signals)
                // fall back to the nominal weight.
                let effective_weight = event
                    .payload
                    .get("gate")
                    .and_then(|g| g.get("effective_weight"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or_else(|| event_kind.update_weight());
                BrainSignal::SemanticFeedback {
                    target_id: target,
                    event_kind,
                    served_by_profile_id: served_by,
                    section_signals,
                    effective_weight,
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
#[path = "event/tests.rs"]
mod tests;
