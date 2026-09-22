//! Knowledge-owned feedback targets and section-learning handoff (#1781).
use khive_brain_core::{resolve_consumer_profile, ConsumerKind, FeedbackSignal, SectionType};
use khive_runtime::{
    EventAttribution, NamespaceToken, RequestIdentity, RuntimeError, VerbRegistry,
};
use khive_storage::event::Event;
use khive_types::{Details, EventKind, KhiveError, SubstrateKind};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::knowledge::{section_feedback::on_section_feedback, KnowledgeHandlers};
use crate::KnowledgePack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackParams {
    target_id: Option<String>,
    signal: Option<String>,
    section_signals: Option<Value>,
    served_by_profile_id: Option<String>,
}

fn parse_signal(value: &str) -> Result<FeedbackSignal, RuntimeError> {
    match value {
        "useful" => Ok(FeedbackSignal::Useful),
        "not_useful" => Ok(FeedbackSignal::NotUseful),
        "wrong" => Ok(FeedbackSignal::Wrong),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown feedback signal {other:?}; expected useful | not_useful | wrong"
        ))),
    }
}

impl KnowledgePack {
    pub(crate) async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: FeedbackParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))?;
        if p.signal.is_none() && p.section_signals.is_none() {
            return Err(KhiveError::invalid_input("signal or section_signals is required").into());
        }
        if let Some(signal) = p.signal.as_deref() {
            parse_signal(signal)?;
        }
        if p.signal.is_some() && p.target_id.is_none() {
            return Err(KhiveError::invalid_input("target_id is required with signal").into());
        }
        let mut signals = Vec::new();
        let mut canonical_sections = serde_json::Map::new();
        if let Some(raw) = p.section_signals.as_ref() {
            let raw = raw
                .as_object()
                .filter(|map| !map.is_empty())
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("section_signals must be a non-empty object".into())
                })?;
            for (key, val) in raw {
                let section = SectionType::from_str_loose(key).ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "unknown section_type: {key:?}; valid: {}",
                        SectionType::NAMES.join(", ")
                    ))
                })?;
                let value = val.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "section signal for {key:?} must be a string"
                    ))
                })?;
                let signal = parse_signal(value)?;
                if canonical_sections
                    .insert(section.as_str().into(), json!(value))
                    .is_some()
                {
                    return Err(KhiveError::invalid_input(format!(
                        "duplicate normalized section: {}",
                        section.as_str()
                    ))
                    .into());
                }
                signals.push((section, signal));
            }
        }
        let (profile, tier) = if let Some(profile) = p
            .served_by_profile_id
            .as_ref()
            .or(self.brain_profile.as_ref())
        {
            if profile.trim().is_empty() {
                return Err(
                    KhiveError::invalid_input("served_by_profile_id must not be empty").into(),
                );
            }
            (Some(profile.clone()), "explicit_profile")
        } else if let Some(profile) =
            resolve_consumer_profile(registry, token, ConsumerKind::KnowledgeCompose).await
        {
            (Some(profile), "bound_profile")
        } else {
            (None, "namespace_local")
        };
        // Explicit profile/section learning must not reintroduce anonymous or
        // unattributed training through the trusted in-process path (#2282).
        let unattributed =
            token.actor().is_anonymous() || khive_runtime::actor_is_unattributed(token.actor());
        if unattributed && (profile.is_some() || !signals.is_empty()) {
            return Err(KhiveError::invalid_input(
                "profile or section feedback requires an attributed caller; configure actor.id",
            )
            .into());
        }
        let target = match p.target_id.as_deref() {
            Some(input) => {
                Some(KnowledgeHandlers::resolve_feedback_target(&self.runtime, input).await?)
            }
            None => None,
        };
        // Reject invalid profile/lifecycle before recording knowledge feedback.
        // The hook repeats this check against its authoritative write snapshot.
        if let Some(profile) = profile.as_deref() {
            let found = registry
                .dispatch_with_identity(
                    "brain.profile",
                    json!({"profile_id":profile,"namespace":token.namespace().as_str()}),
                    Some(RequestIdentity::from_token(token)),
                )
                .await?;
            if found["lifecycle"]
                .as_str()
                .is_some_and(|state| state.eq_ignore_ascii_case("archived"))
            {
                return Err(KhiveError::invalid_input(
                    "section feedback cannot credit an archived profile",
                )
                .into());
            }
        }
        let mut payload = json!({
            "signal": p.signal,
            "served_by_profile_id": profile,
            "target_kind": target.map(|(_, kind)| kind),
            "target_id": target.map(|(id, _)| id.to_string()),
        });
        if !signals.is_empty() {
            payload["section_signals"] = Value::Object(canonical_sections.clone());
        }
        let mut event = Event::new(
            token.namespace().as_str(),
            "knowledge.feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_payload(payload);
        event.target_id = target.map(|(id, _)| id);
        let event = EventAttribution::from_token(token).stamp(event);
        let event_id = event.id;
        self.runtime.events(token)?.append_event(event).await?;
        let mut total_events = None;
        let mut profile_event_id = Value::Null;
        if !signals.is_empty() {
            if let Some(profile) = profile.as_deref() {
                let attribution = target.map(|(id, kind)| format!("{kind}:{id}"));
                match registry
                    .apply_profile_section_feedback(
                        token,
                        profile,
                        Value::Object(canonical_sections),
                        attribution,
                    )
                    .await
                {
                    Ok(result) => {
                        profile_event_id = result["event_id"].clone();
                    }
                    Err(error) => {
                        // Across independent backends, the earlier append cannot be
                        // rolled back. The outer dispatch disposition remains unknown;
                        // report the known component commit without claiming full success.
                        tracing::warn!(%event_id, %error, "knowledge feedback recorded but profile section update failed");
                        return Err(KhiveError::internal("knowledge feedback event committed; profile section update was not confirmed; inspect event before retrying")
                            .with_details(Details::new_owned([
                                ("knowledge_event_id", event_id.to_string()),
                                ("knowledge_event_disposition", "committed".into()),
                                ("profile_update_disposition", "unknown".into()),
                            ])).into());
                    }
                }
            } else {
                let mut states = self.section_posteriors.lock().map_err(|_| {
                    RuntimeError::Internal("section_posteriors lock poisoned".into())
                })?;
                let state = states.entry(token.namespace().as_str().into()).or_default();
                on_section_feedback(state, &signals);
                total_events = Some(state.total_events);
            }
        }
        Ok(
            json!({"ok":true, "emitted":true, "event_id":event_id.to_string(), "tier":tier,
            "target_id_used":target.is_some(), "target_id":target.map(|(id,_)|id.to_string()),
            "target_kind":target.map(|(_,kind)|kind), "signal":p.signal, "brain_profile":profile,
            "signals_applied":signals.len(), "total_events":total_events, "profile_event_id":profile_event_id}),
        )
    }
}
