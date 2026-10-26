use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};
use uuid::Uuid;

const PROFILE: &str = "balanced-recall-v1";

async fn feedback_registry(with_profile: bool) -> (VerbRegistry, Uuid) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        events_split: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        brain_profile: with_profile.then(|| PROFILE.to_string()),
        packs: if with_profile {
            vec!["kg".to_string(), "memory".to_string(), "brain".to_string()]
        } else {
            vec!["kg".to_string(), "memory".to_string()]
        },
        ..RuntimeConfig::default()
    })
    .expect("private feedback runtime");
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let token = runtime
        .authorize(Namespace::parse("local").expect("namespace"))
        .expect("token");
    let note = runtime
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "memory feedback parameter contract target",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("real feedback target");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("memory-feedback-caller".to_string()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(crate::MemoryPack::new(runtime.clone()));
    if with_profile {
        builder.register(BrainPack::new(runtime));
    }
    (builder.build().expect("feedback registry"), note.id)
}

async fn stored_target(registry: &VerbRegistry, id: Uuid) -> Value {
    let record = registry
        .dispatch(
            "get",
            json!({
                "namespace": "local",
                "id": id.to_string(),
            }),
        )
        .await
        .expect("the real feedback target remains readable");
    assert!(
        record.is_object(),
        "get must return the stored target: {record}"
    );
    record
}

async fn profile_events(registry: &VerbRegistry) -> u64 {
    let profile = registry
        .dispatch(
            "brain.profile",
            json!({
                "namespace": "local",
                "profile_id": PROFILE,
            }),
        )
        .await
        .expect("the configured profile is readable");
    profile["total_events"]
        .as_u64()
        .expect("profile total_events must be an integer")
}

fn invalid_input(error: RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(message) => message,
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn feedback_help_prefers_required_id_and_documents_target_id_alias() {
    let (registry, _) = feedback_registry(false).await;
    let help = registry
        .dispatch("memory.feedback", json!({"help": true}))
        .await
        .expect("public help");
    let params = help["params"].as_array().expect("help params");
    let canonical: Vec<_> = params
        .iter()
        .filter(|param| param["name"] == "id")
        .collect();
    assert_eq!(canonical.len(), 1);
    assert_eq!(canonical[0]["type"], "uuid");
    assert_eq!(canonical[0]["required"], true);
    assert!(canonical[0]["description"]
        .as_str()
        .expect("id description")
        .contains("target_id"));
    assert!(!params.iter().any(|param| param["name"] == "target_id"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn feedback_id_and_target_id_keep_the_canonical_tier3_response() {
    let (registry, id) = feedback_registry(false).await;
    let stored = stored_target(&registry, id).await;
    let canonical = id.as_hyphenated().to_string();
    let mut responses = Vec::new();
    for key in ["id", "target_id"] {
        let mut params = json!({"namespace": "local", "signal": "useful"});
        params[key] = json!(format!("{{{}}}", canonical.to_uppercase()));
        let response = registry
            .dispatch("memory.feedback", params)
            .await
            .expect("full UUID feedback");
        assert_eq!(
            response,
            json!({"ok": true, "target_id": canonical, "signal": "useful"})
        );
        assert_eq!(stored_target(&registry, id).await, stored);
        responses.push(response);
    }
    assert_eq!(responses[0], responses[1]);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn feedback_both_id_spellings_update_the_configured_profile() {
    let (registry, id) = feedback_registry(true).await;
    let stored = stored_target(&registry, id).await;
    let canonical = id.as_hyphenated().to_string();
    assert_eq!(profile_events(&registry).await, 0);
    for (index, key) in ["id", "target_id"].into_iter().enumerate() {
        let mut params = json!({"namespace": "local", "signal": "useful"});
        params[key] = json!(format!("{{{}}}", canonical.to_uppercase()));
        let response = registry
            .dispatch("memory.feedback", params)
            .await
            .expect("profile feedback");
        assert_eq!(response["emitted"], true);
        assert_eq!(response["target_id"], canonical);
        assert_eq!(profile_events(&registry).await, index as u64 + 1);
        assert_eq!(stored_target(&registry, id).await, stored);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn feedback_duplicate_ids_and_invalid_values_refuse_before_profile_updates() {
    let (registry, id) = feedback_registry(true).await;
    let canonical = id.as_hyphenated().to_string();
    let healthy = registry
        .dispatch(
            "memory.feedback",
            json!({
                "namespace": "local", "id": canonical, "signal": "useful",
            }),
        )
        .await
        .expect("healthy canonical feedback before refusal checks");
    assert_eq!(healthy["emitted"], true);
    assert_eq!(healthy["target_id"], canonical);
    let baseline_events = profile_events(&registry).await;
    assert_eq!(baseline_events, 1);
    let stored = stored_target(&registry, id).await;

    let error = registry
        .dispatch(
            "memory.feedback",
            json!({
                "namespace": "local", "id": canonical, "target_id": canonical, "signal": "useful",
            }),
        )
        .await
        .expect_err("both spellings are ambiguous even when their UUID strings agree");
    let message = invalid_input(error);
    assert!(
        message.starts_with("memory.feedback: invalid params:"),
        "{message}"
    );
    assert!(
        message.contains("duplicate field") && message.contains("target_id"),
        "{message}"
    );
    assert_eq!(profile_events(&registry).await, baseline_events);
    assert_eq!(stored_target(&registry, id).await, stored);

    for key in ["id", "target_id"] {
        let mut params = json!({"namespace": "local", "signal": "useful"});
        params[key] = json!("deadbeef");
        let error = registry
            .dispatch("memory.feedback", params)
            .await
            .expect_err("a prefix is not a full UUID");
        let message = invalid_input(error);
        assert!(
            message.starts_with("memory.feedback: target_id must be a full UUID"),
            "{message}"
        );
        assert!(
            message.contains("can miss or be ambiguous")
                && message.contains("one exact recalled record"),
            "{message}"
        );
        assert_eq!(profile_events(&registry).await, baseline_events);
        assert_eq!(stored_target(&registry, id).await, stored);

        let mut params = json!({"namespace": "local", "signal": "bad_value"});
        params[key] = json!(canonical);
        let error = registry
            .dispatch("memory.feedback", params)
            .await
            .expect_err("unknown feedback signal");
        let message = invalid_input(error);
        assert!(
            message.starts_with("memory.feedback: invalid signal"),
            "{message}"
        );
        assert_eq!(profile_events(&registry).await, baseline_events);
        assert_eq!(stored_target(&registry, id).await, stored);
    }
}
