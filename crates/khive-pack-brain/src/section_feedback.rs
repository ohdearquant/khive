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
    Ok((profile, parse_signals(signals)?))
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
        khive_storage::usage::count(khive_storage::usage::UsageUnit::EventRows, 1);
        Ok(json!({
            "emitted": true,
            "event_id": event.id,
            "served_by_profile_id": profile_id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{Namespace, PackRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
    use khive_storage::EventFilter;

    fn runtime(actor: Option<&str>) -> khive_runtime::KhiveRuntime {
        khive_runtime::KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: actor.map(str::to_owned),
            brain_profile: None,
            packs: vec!["kg".into(), "brain".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap()
    }

    fn registry(runtime: &khive_runtime::KhiveRuntime) -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        khive_runtime::PackRegistry::register_packs(
            &["kg".into(), "brain".into()],
            runtime.clone(),
            &mut builder,
        )
        .unwrap();
        builder.with_actor_id(runtime.config().actor_id.clone());
        builder.build().unwrap()
    }

    async fn profile(registry: &VerbRegistry, id: &str) -> Value {
        registry
            .dispatch("brain.profile", json!({"profile_id":id}))
            .await
            .unwrap()
    }

    async fn create_profile(registry: &VerbRegistry, name: &str) {
        let sections: serde_json::Map<String, Value> = SectionType::all()
            .iter()
            .map(|section| (section.as_str().into(), json!({"alpha":2.0,"beta":2.0})))
            .collect();
        registry
            .dispatch(
                "brain.create_profile",
                json!({
                    "name":name,
                    "consumer_kind":"knowledge_compose",
                    "seed_priors":{"section_posteriors":sections}
                }),
            )
            .await
            .unwrap();
    }

    async fn durable_snapshot(runtime: &khive_runtime::KhiveRuntime) -> Value {
        let snapshot = crate::persist::load_latest_snapshot(
            runtime.sql().as_ref(),
            "local",
            ENTITY_CACHE_CAPACITY,
        )
        .await
        .unwrap();
        json!(snapshot)
    }

    async fn section_event_count(
        runtime: &khive_runtime::KhiveRuntime,
        token: &NamespaceToken,
    ) -> u64 {
        runtime
            .events(token)
            .unwrap()
            .count_events(EventFilter {
                verbs: vec!["brain.section_feedback".into()],
                ..Default::default()
            })
            .await
            .unwrap()
    }

    // MUST-FAIL: forwarding this opaque id to brain.feedback, using a synthetic
    // KG UUID, skipping durable mutation, or replaying only interpret(event).
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn section_feedback_factory_hook_persists_and_replays_only_selected_sections() {
        let runtime = runtime(Some("test:knowledge"));
        let registry = registry(&runtime);
        let token = runtime.authorize(Namespace::local()).unwrap();
        create_profile(&registry, "knowledge-profile").await;
        let before = profile(&registry, "knowledge-profile").await;
        let default_before = profile(&registry, "balanced-recall-v1").await;
        let (old_snapshot, old_version) = crate::persist::load_latest_snapshot(
            runtime.sql().as_ref(),
            "local",
            ENTITY_CACHE_CAPACITY,
        )
        .await
        .unwrap()
        .unwrap();
        let result = registry
            .apply_profile_section_feedback(
                &token,
                "knowledge-profile",
                json!({"overview":"useful", "core_model":"wrong"}),
                Some("atom:not-a-kg-uuid".into()),
            )
            .await
            .unwrap();
        assert_eq!(result["emitted"], true);
        assert_eq!(result["served_by_profile_id"], "knowledge-profile");
        let after = profile(&registry, "knowledge-profile").await;
        assert_eq!(after["section_posteriors"]["overview"]["alpha"], json!(3.0));
        assert_eq!(
            after["section_posteriors"]["core_model"]["beta"],
            json!(4.0)
        );
        assert!(
            after["section_posteriors"]["overview"]["weight"]
                .as_f64()
                .unwrap()
                > before["section_posteriors"]["overview"]["weight"]
                    .as_f64()
                    .unwrap()
        );
        assert_eq!(
            after["state_snapshot"], before["state_snapshot"],
            "section evidence must not update per-target/global recall posteriors"
        );
        assert_eq!(after["total_events"], before["total_events"]);
        assert_eq!(
            profile(&registry, "balanced-recall-v1").await,
            default_before
        );
        let id = uuid::Uuid::parse_str(result["event_id"].as_str().unwrap()).unwrap();
        let event = runtime
            .events(&token)
            .unwrap()
            .get_event(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.verb, "brain.section_feedback");
        assert_eq!(event.target_id, None);
        assert_eq!(event.payload["target_attribution"], "atom:not-a-kg-uuid");
        assert_eq!(event.payload["served_by_profile_id"], "knowledge-profile");
        assert!(event.payload.get("signal").is_none());
        assert!(event.payload.get("target_id").is_none());
        let log = crate::persist::load_events_since(runtime.sql().as_ref(), "local", old_version)
            .await
            .unwrap();
        assert_eq!(log.events, vec![event.clone()]);
        assert!(log.quarantined.is_empty());
        let reloaded = self::registry(&runtime);
        assert_eq!(
            profile(&reloaded, "knowledge-profile").await,
            after,
            "fresh instance restores the durable snapshot"
        );
        // Restore the actual pre-feedback snapshot to force log replay. Merely
        // reopening the final snapshot would not exercise the new replay arm.
        crate::persist::upsert_snapshot(
            runtime.sql().as_ref(),
            "local",
            &old_snapshot,
            old_version,
        )
        .await
        .unwrap();
        let replayed = self::registry(&runtime);
        assert_eq!(profile(&replayed, "knowledge-profile").await, after);
        assert_eq!(
            profile(&replayed, "balanced-recall-v1").await,
            default_before
        );
        assert_eq!(section_event_count(&runtime, &token).await, 1);
        assert!(!registry
            .all_handlers_with_names()
            .iter()
            .any(|(_, handler)| handler.name == "brain.section_feedback"));
        registry
            .dispatch(
                "brain.section_feedback",
                json!({"profile_id":"knowledge-profile", "section_signals":{"overview":"useful"}}),
            )
            .await
            .expect_err("trusted hook is not a wire-dispatch handler");
        assert_eq!(section_event_count(&runtime, &token).await, 1);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn section_feedback_rejects_invalid_sections_and_profile_lifecycle_without_writes() {
        let runtime = runtime(Some("test:knowledge"));
        let registry = registry(&runtime);
        let token = runtime.authorize(Namespace::local()).unwrap();
        create_profile(&registry, "archived-profile").await;
        registry
            .dispatch("brain.archive", json!({"profile_id":"archived-profile"}))
            .await
            .unwrap();
        for (profile, signals) in [
            ("missing-profile", json!({"overview":"useful"})),
            ("archived-profile", json!({"overview":"useful"})),
            ("balanced-recall-v1", json!({})),
            ("balanced-recall-v1", json!({"unknown":"useful"})),
            (
                "balanced-recall-v1",
                json!({"overview":"implicit_positive"}),
            ),
            ("balanced-recall-v1", json!({"overview":true})),
        ] {
            let before = durable_snapshot(&runtime).await;
            registry
                .apply_profile_section_feedback(&token, profile, signals, None)
                .await
                .expect_err("invalid section/profile evidence must refuse");
            assert_eq!(durable_snapshot(&runtime).await, before);
            assert_eq!(section_event_count(&runtime, &token).await, 0);
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn section_feedback_refuses_unattributed_callers_before_load() {
        // Both the anonymous actor and the configured "local" pool are
        // unattributed; neither may train a profile or bootstrap its state.
        for actor in [None, Some("local")] {
            let unattributed = runtime(actor);
            let pack = BrainPack::new(unattributed.clone());
            let token = unattributed.authorize(Namespace::local()).unwrap();
            let before = durable_snapshot(&unattributed).await;
            let error = pack
                .apply_profile_section_feedback(
                    &token,
                    "balanced-recall-v1",
                    json!({"overview":"useful"}),
                    None,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("attributed actor")),
                "{actor:?}: {error:?}"
            );
            assert!(!pack.persistence.lock().unwrap().is_loaded("local"));
            assert_eq!(durable_snapshot(&unattributed).await, before);
            assert_eq!(section_event_count(&unattributed, &token).await, 0);
        }
        let named = runtime(Some("test:knowledge"));
        let token = named.authorize(Namespace::local()).unwrap();
        registry(&named)
            .apply_profile_section_feedback(
                &token,
                "balanced-recall-v1",
                json!({"overview":"not_useful"}),
                Some("domain:opaque".into()),
            )
            .await
            .unwrap();
        assert_eq!(section_event_count(&named, &token).await, 1);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn section_feedback_snapshot_failure_rolls_back_public_event_log_and_state() {
        let runtime = runtime(Some("test:knowledge"));
        let registry = registry(&runtime);
        let token = runtime.authorize(Namespace::local()).unwrap();
        create_profile(&registry, "knowledge-profile").await;
        let before = durable_snapshot(&runtime).await;
        let before_profile = profile(&registry, "knowledge-profile").await;
        let (_, version) = crate::persist::load_latest_snapshot(
            runtime.sql().as_ref(),
            "local",
            ENTITY_CACHE_CAPACITY,
        )
        .await
        .unwrap()
        .unwrap();
        runtime.sql().writer().await.unwrap().execute_script("CREATE TRIGGER refuse_section_snapshot BEFORE INSERT ON brain_profile_snapshots BEGIN SELECT RAISE(ABORT, 'section snapshot failure'); END;".into()).await.unwrap();
        registry
            .apply_profile_section_feedback(
                &token,
                "knowledge-profile",
                json!({"overview":"useful"}),
                Some("domain:test".into()),
            )
            .await
            .expect_err("snapshot failure must not publish either event or live state");
        assert_eq!(durable_snapshot(&runtime).await, before);
        assert_eq!(
            profile(&registry, "knowledge-profile").await,
            before_profile
        );
        assert_eq!(section_event_count(&runtime, &token).await, 0);
        assert!(
            crate::persist::load_events_since(runtime.sql().as_ref(), "local", version)
                .await
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn section_feedback_replay_quarantines_invalid_target_or_sections() {
        let runtime = runtime(Some("test:knowledge"));
        let registry = registry(&runtime);
        create_profile(&registry, "knowledge-profile").await;
        let (_, version) = crate::persist::load_latest_snapshot(
            runtime.sql().as_ref(),
            "local",
            ENTITY_CACHE_CAPACITY,
        )
        .await
        .unwrap()
        .unwrap();
        for (offset, payload, target) in [
            (
                1,
                json!({"served_by_profile_id":"knowledge-profile","section_signals":{"bad":"useful"}}),
                None,
            ),
            (
                2,
                json!({"served_by_profile_id":"knowledge-profile","section_signals":{"overview":"useful"}}),
                Some(uuid::Uuid::new_v4()),
            ),
            (3, json!({"section_signals":{"overview":"useful"}}), None),
        ] {
            let mut event = Event::new(
                "local",
                "brain.section_feedback",
                EventKind::FeedbackExplicit,
                SubstrateKind::Event,
                "actor:test:knowledge",
            )
            .with_payload(payload);
            event.target_id = target;
            event.created_at = version + offset;
            crate::persist::append_brain_event(
                runtime.sql().as_ref(),
                "local",
                "knowledge-profile",
                "brain.section_feedback",
                &json!(event),
                version + offset,
            )
            .await
            .unwrap();
        }
        let replay = crate::persist::load_events_since(runtime.sql().as_ref(), "local", version)
            .await
            .unwrap();
        assert!(replay.events.is_empty());
        assert_eq!(replay.quarantined.len(), 3);
        let before = profile(&registry, "knowledge-profile").await;
        assert_eq!(
            profile(&self::registry(&runtime), "knowledge-profile").await,
            before
        );
    }
}
