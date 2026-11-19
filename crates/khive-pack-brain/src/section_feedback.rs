//! Trusted section evidence from packs whose targets are outside the KG.

use std::collections::HashMap;

use khive_brain_core::{BrainState, FeedbackSignal, ProfileLifecycle, SectionType};
use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::event::Event;
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};

use crate::{BrainPack, ENTITY_CACHE_CAPACITY};

type SectionSignals = HashMap<SectionType, FeedbackSignal>;

fn parse_signals(signals: &Value) -> Result<SectionSignals, RuntimeError> {
    crate::validate_section_signals(signals)?;
    serde_json::from_value(signals.clone())
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
}

pub(crate) fn decode_event(event: &Event) -> Result<(&str, SectionSignals), RuntimeError> {
    if event.verb != "brain.section_feedback"
        || event.target_id.is_some()
        || event.kind != EventKind::FeedbackExplicit
        || event.outcome != EventOutcome::Success
    {
        return Err(RuntimeError::InvalidInput(
            "invalid target-free section feedback event".into(),
        ));
    }
    let profile = event
        .payload
        .get("served_by_profile_id")
        .and_then(Value::as_str)
        .filter(|profile| !profile.trim().is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput("section feedback requires served_by_profile_id".into())
        })?;
    if event
        .payload
        .get("target_attribution")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err(RuntimeError::InvalidInput(
            "section feedback target_attribution must be an opaque string".into(),
        ));
    }
    let signals = event.payload.get("section_signals").ok_or_else(|| {
        RuntimeError::InvalidInput("section feedback requires section_signals".into())
    })?;
    // Replay-only: entries keyed by a retired section type are dropped and the rest
    // apply. A map that held only retired entries decodes to no signals. Live writes
    // call `parse_signals` directly and refuse a retired name as unknown.
    let signals = match crate::replay_section_signals(signals) {
        Some(kept) => parse_signals(&kept)?,
        None => SectionSignals::new(),
    };
    Ok((profile, signals))
}

pub(crate) fn replay(state: &mut BrainState, event: &Event) -> Result<(), RuntimeError> {
    let (profile, signals) = decode_event(event)?;
    match state.profiles.get(profile) {
        None => {
            return Err(RuntimeError::NotFound(format!(
                "section feedback profile {profile:?} not found"
            )))
        }
        Some(record) if record.lifecycle == ProfileLifecycle::Archived => {
            return Err(RuntimeError::InvalidInput(format!(
                "section feedback profile {profile:?} is archived"
            )));
        }
        Some(_) => {}
    }
    // An event whose signals were all retired carries no evidence left to apply; applying
    // an empty map would still advance the event count and the exploration epoch.
    if signals.is_empty() {
        return Ok(());
    }
    crate::ensure_section_state_seeded(&mut state.section_states, profile)
        .apply_section_signals(&signals);
    Ok(())
}

impl BrainPack {
    pub(crate) async fn apply_section_feedback(
        &self,
        token: &NamespaceToken,
        profile_id: &str,
        section_signals: Value,
        target_attribution: Option<String>,
    ) -> Result<Value, RuntimeError> {
        // No persistence/bootstrap side effect may precede the explicit-feedback
        // attribution guard, even for a direct trusted-Rust call. The "local"
        // actor is the unattributed pool and is refused like the anonymous one.
        if token.actor().is_anonymous() || khive_runtime::actor_is_unattributed(token.actor()) {
            return Err(RuntimeError::InvalidInput(
                "profile section feedback requires an attributed actor".into(),
            ));
        }
        let signals = parse_signals(&section_signals)?;
        let _gate = self.dispatch_gate.lock().await;
        self.ensure_loaded(token).await?;
        let profile = profile_id.to_owned();
        let event = Event::new(
            token.namespace().as_str(),
            "brain.section_feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_payload(json!({
            "originating_verb": "knowledge.feedback",
            "served_by_profile_id": profile_id,
            "section_signals": section_signals,
            "target_attribution": target_attribution,
        }));
        let event = crate::persist::persist_feedback_state_mutation(
            self.runtime.sql().as_ref(),
            token,
            &self.persistence,
            &self.state,
            profile.clone(),
            crate::persist::FeedbackEventWrite::Direct(event),
            ENTITY_CACHE_CAPACITY,
            move |state, _| {
                // This update carries only section evidence. In particular, it
                // must not fabricate a useful vote for a KG target/profile.
                crate::ensure_section_state_seeded(&mut state.section_states, &profile)
                    .apply_section_signals(&signals);
            },
        )
        .await?
        .ok_or_else(|| {
            RuntimeError::InvalidInput("direct section feedback unexpectedly deduplicated".into())
        })?;
        Ok(json!({
            "emitted": true,
            "event_id": event.id,
            "served_by_profile_id": profile_id,
        }))
    }
}

#[cfg(test)]
#[path = "section_feedback/tests.rs"]
mod tests;
