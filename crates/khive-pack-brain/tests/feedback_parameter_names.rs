//! Canonical brain.feedback input naming preserves its event and response contracts.
use std::collections::HashMap;

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{event::EventFilter, types::PageRequest};
use khive_types::EventKind;
use serde_json::{json, Value};
use uuid::Uuid;

const PROFILE: &str = "feedback-alias-profile";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = None;
        config.events_split = None;
        config.actor_id = Some("brain-feedback-alias-fixture".into());
        config.brain_profile = None;
        config.packs = vec!["kg".into(), "brain".into()];
        config.volume_lock_dir = None;
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
        config.wal_ceiling_env_raw = None;
        config.disk_guard_environment = Default::default();
        config.disk_guard_config = None;
        config.credentials.clear();
        config.visibility_receipts = None;
        config.mounts.clear();
        let runtime = KhiveRuntime::new(config).expect("private memory runtime");
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("brain-feedback-alias-fixture".into()));
        builder.register(KgPack::new(runtime.clone()));
        builder.register(BrainPack::new(runtime.clone()));
        let registry = builder.build().expect("real KG and brain registry");
        registry
            .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
            .expect("actual pack schema");
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn profile(&self) -> Value {
        self.registry
            .dispatch("brain.profile", json!({"profile_id":PROFILE}))
            .await
            .expect("actual profile read")
    }

    async fn feedback_events(&self) -> Value {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let page = self
            .runtime
            .events(&token)
            .unwrap()
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::FeedbackExplicit],
                    ..EventFilter::default()
                },
                PageRequest {
                    limit: 1000,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert!(
            page.items.len() < 1000,
            "feedback snapshot must be complete"
        );
        json!(page.items)
    }

    async fn target(&self, name: &str) -> Uuid {
        let value = self
            .registry
            .dispatch(
                "create",
                json!({"kind":"concept", "name":name, "skip_dedup_check":true}),
            )
            .await
            .expect("real live feedback target");
        let raw = value["id"].as_str().expect("created target UUID");
        let id = Uuid::parse_str(raw).unwrap();
        assert_eq!(id.to_string(), raw);
        id
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn brain_feedback_help_prefers_id_and_documents_target_id_alias() {
    let f = Fixture::new();
    let help = f
        .registry
        .dispatch("brain.feedback", json!({"help":true}))
        .await
        .expect("real public feedback help");
    assert_eq!(help["verb"], "brain.feedback");
    assert_eq!(help["pack"], "brain");
    let params = help["params"].as_array().unwrap();
    assert_eq!(
        params.iter().filter(|param| param["name"] == "id").count(),
        1
    );
    assert!(!params.iter().any(|param| param["name"] == "target_id"));
    let id = params.iter().find(|param| param["name"] == "id").unwrap();
    assert_eq!(id["type"], "uuid");
    assert_eq!(id["required"], true);
    let description = id["description"].as_str().unwrap();
    assert!(
        description.contains("target_id"),
        "legacy input alias stays documented"
    );
    assert!(
        description.contains("both"),
        "simultaneous names must be documented as refused"
    );
    assert!(description.contains("Complete UUID or globally unique 8+ hex prefix"));
    assert!(
        description.contains("namespace filter at all"),
        "unscoped identifier resolution stays unchanged"
    );
    assert!(params
        .iter()
        .any(|param| param["name"] == "signal" && param["required"] == true));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn brain_feedback_aliases_preserve_events_and_reject_both_names() {
    let f = Fixture::new();
    let target = f.target("Canonical and legacy feedback target").await;
    let other = f.target("Distinct duplicate-field feedback target").await;
    assert_ne!(target, other);
    f.registry
        .dispatch(
            "brain.create_profile",
            json!({"name":PROFILE, "consumer_kind":"recall"}),
        )
        .await
        .expect("real attributed profile");
    f.registry
        .dispatch("brain.activate", json!({"profile_id":PROFILE}))
        .await
        .expect("active attributed profile");
    let initial = f.profile().await;
    assert_eq!(initial["id"], PROFILE);
    let mut total = initial["total_events"].as_u64().unwrap();
    assert_eq!(f.feedback_events().await, json!([]));
    let mut stable_responses = Vec::new();
    let mut emitted_ids = Vec::new();

    for (index, spelling) in ["id", "target_id"].into_iter().enumerate() {
        let mut args = json!({"signal":"useful", "served_by_profile_id":PROFILE});
        args[spelling] = json!(target);
        let response = f
            .registry
            .dispatch("brain.feedback", args)
            .await
            .expect("each accepted spelling must perform real feedback");
        let mut keys: Vec<_> = response
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "emitted",
                "event_id",
                "serve_attribution",
                "served_by_profile_id",
                "signal",
                "target_id",
                "verb"
            ]
        );
        assert_eq!(response["emitted"], true);
        assert_eq!(response["verb"], "brain.feedback");
        assert_eq!(response["target_id"], target.to_string());
        assert_eq!(response["signal"], "useful");
        assert_eq!(response["served_by_profile_id"], PROFILE);
        assert_eq!(response["serve_attribution"], "profile");
        let event_id = Uuid::parse_str(response["event_id"].as_str().unwrap()).unwrap();
        assert!(!emitted_ids.contains(&event_id));
        emitted_ids.push(event_id);
        let token = f.runtime.authorize(Namespace::local()).unwrap();
        let event = f
            .runtime
            .events(&token)
            .unwrap()
            .get_event(event_id)
            .await
            .unwrap()
            .expect("returned feedback event must be persisted");
        assert_eq!(event.kind, EventKind::FeedbackExplicit);
        assert_eq!(event.verb, "brain.feedback");
        assert_eq!(event.namespace, "local");
        assert_eq!(event.target_id, Some(target));
        assert_eq!(event.payload["signal"], "useful");
        assert_eq!(event.payload["originating_verb"], "brain.feedback");
        assert_eq!(event.payload["served_by_profile_id"], PROFILE);
        assert_eq!(
            event.payload["serve_attribution"],
            response["serve_attribution"]
        );
        assert_eq!(
            f.feedback_events().await.as_array().unwrap().len(),
            index + 1
        );
        let after = f.profile().await;
        let next = after["total_events"].as_u64().unwrap();
        assert!(
            next > total,
            "each spelling must reach the real profile mutation"
        );
        total = next;
        let mut stable = response;
        stable.as_object_mut().unwrap().remove("event_id");
        stable_responses.push(stable);
    }
    assert_eq!(stable_responses[0], stable_responses[1]);

    let before_profile = f.profile().await;
    let before_events = f.feedback_events().await;
    assert_eq!(before_events.as_array().unwrap().len(), 2);
    for legacy in [target, other] {
        let error = f
            .registry
            .dispatch(
                "brain.feedback",
                json!({
                    "id":target, "target_id":legacy,
                    "signal":"useful", "served_by_profile_id":PROFILE
                }),
            )
            .await
            .expect_err("equal and different simultaneous names must be refused");
        let RuntimeError::InvalidInput(message) = &error else {
            panic!("wrong duplicate-parameter refusal kind: {error:?}");
        };
        assert_eq!(message, "duplicate field `id`");
        assert_eq!(error.to_string(), format!("invalid input: {message}"));
        assert_eq!(
            f.profile().await,
            before_profile,
            "refusal must not mutate the warmed profile"
        );
        assert_eq!(
            f.feedback_events().await,
            before_events,
            "refusal must not append a FeedbackExplicit row"
        );
    }
}
