//! Tool list discovery describes the actor predicates used by real dispatch.

use std::collections::BTreeSet;

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{
    KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder, WalCeilingSource,
};
use serde_json::{json, Value};

fn registry() -> VerbRegistry {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into(), "tool".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("isolated in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("catalog-operator".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and tool registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("tool schema on the private memory backend");
    registry
}

fn actors(response: &Value, key: &str) -> BTreeSet<String> {
    assert_eq!(response["ok"], true, "{response}");
    let rows = response[key].as_array().expect("list rows");
    assert_eq!(response["count"], json!(rows.len()), "{response}");
    rows.iter()
        .map(|row| row["actor"].as_str().expect("actor label").to_owned())
        .collect()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_actor_help_matches_named_caller_dispatch() {
    let registry = registry();
    for (verb, description) in [
        (
            "tool.requests",
            "Optional exact actor filter. Omit to apply no actor filter within the request namespace.",
        ),
        (
            "tool.policies",
            "Optional actor value to match against stored policy patterns. Omit to apply no actor filter within the request namespace.",
        ),
    ] {
        let help = registry.describe_verb(verb).expect("real verb discovery");
        let params = help["params"].as_array().expect("parameter metadata");
        let actor: Vec<_> = params.iter().filter(|param| param["name"] == "actor").collect();
        assert_eq!(actor.len(), 1, "{help}");
        assert_eq!(actor[0]["type"], "string");
        assert_eq!(actor[0]["required"], false);
        assert_eq!(actor[0]["description"], description);
        let schema = &help["input_schema"];
        assert_eq!(schema["properties"]["actor"]["type"], "string");
        assert_eq!(schema["properties"]["actor"]["description"], description);
        assert_eq!(schema["required"], json!([]));
        assert_eq!(schema["additionalProperties"], true);
    }
    for verb in [
        "tool.suggest",
        "tool.describe",
        "tool.check",
        "tool.request",
    ] {
        let help = registry
            .describe_verb(verb)
            .expect("decision verb discovery");
        let actor = help["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|param| param["name"] == "actor")
            .expect("decision actor parameter");
        assert_eq!(
            actor["description"],
            "Actor label the decision is evaluated for (defaults to the caller's actor)."
        );
    }

    // An unregistered tool defaults to ask, so each real request persists a row.
    for actor in ["agent:alpha", "agent:beta", "agent:*"] {
        let request = registry
            .dispatch(
                "tool.request",
                json!({"tool": "approval-target", "actor": actor}),
            )
            .await
            .expect("request for another actor");
        assert_eq!(request["actor"], actor);
        assert_eq!(request["status"], "requested");
        assert_eq!(request["registered"], false);
        assert!(request["request_id"].is_string());
    }
    let all = registry.dispatch("tool.requests", json!({})).await.unwrap();
    assert_eq!(
        actors(&all, "requests"),
        BTreeSet::from(["agent:*".into(), "agent:alpha".into(), "agent:beta".into()])
    );
    let exact = registry
        .dispatch("tool.requests", json!({"actor": "agent:alpha"}))
        .await
        .unwrap();
    assert_eq!(
        actors(&exact, "requests"),
        BTreeSet::from(["agent:alpha".into()])
    );
    let pattern = registry
        .dispatch("tool.requests", json!({"actor": "agent:*"}))
        .await
        .unwrap();
    assert_eq!(
        actors(&pattern, "requests"),
        BTreeSet::from(["agent:*".into()])
    );

    for (actor, decision) in [
        ("agent:*", "deny"),
        ("agent:alpha", "allow"),
        ("other:beta", "ask"),
    ] {
        let policy = registry
            .dispatch(
                "tool.policy",
                json!({"actor": actor, "tool": "policy-target", "decision": decision}),
            )
            .await
            .expect("distinct policy label");
        assert_eq!(policy["policy"]["actor"], actor);
    }
    let all = registry.dispatch("tool.policies", json!({})).await.unwrap();
    assert_eq!(
        actors(&all, "policies"),
        BTreeSet::from(["agent:*".into(), "agent:alpha".into(), "other:beta".into()])
    );
    let matching = registry
        .dispatch("tool.policies", json!({"actor": "agent:alpha"}))
        .await
        .unwrap();
    assert_eq!(
        actors(&matching, "policies"),
        BTreeSet::from(["agent:*".into(), "agent:alpha".into()])
    );
    let caller = registry
        .dispatch("tool.policies", json!({"actor": "catalog-operator"}))
        .await
        .unwrap();
    assert!(actors(&caller, "policies").is_empty());
}
