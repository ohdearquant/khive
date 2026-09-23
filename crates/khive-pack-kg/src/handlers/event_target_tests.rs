use crate::handlers::common::{event_filter_from_params, ListParams};
use crate::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{Event, EventOutcome};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, KgPack, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        actor_id: Some("actor:event-target-test".into()),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let pack = KgPack::new(runtime.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, pack, registry)
}

#[test]
fn event_target_requires_a_full_uuid_without_name_or_prefix_resolution() {
    let target = Uuid::new_v4();
    for raw in [
        target.simple().to_string()[..8].to_owned(),
        "named-atom".into(),
        "".into(),
    ] {
        let params: ListParams =
            serde_json::from_value(json!({"kind": "event", "target_id": raw})).unwrap();
        let error = event_filter_from_params(&params).unwrap_err();
        assert!(error
            .to_string()
            .contains("target_id must contain a full UUID"));
    }
    let params: ListParams =
        serde_json::from_value(json!({"kind": "event", "target_id": target})).unwrap();
    assert_eq!(
        event_filter_from_params(&params).unwrap().0.target_id,
        Some(target)
    );
}

#[tokio::test]
async fn event_target_lists_refusals_in_the_authorized_namespace_without_graph_records() {
    let (runtime, pack, registry) = fixture();
    let local = runtime.authorize(Namespace::local()).unwrap();
    let other = runtime
        .authorize(Namespace::parse("other").unwrap())
        .unwrap();
    // No entity or note has this UUID: a knowledge subject is not a graph anchor.
    let subject = Uuid::new_v4();
    let make = |target, kind| {
        Event::new(
            "ignored",
            "knowledge.upsert_atoms",
            kind,
            SubstrateKind::Event,
            "ignored",
        )
        .with_target(target)
        .with_outcome(EventOutcome::Denied)
    };
    let wanted = make(subject, EventKind::Refusal);
    runtime
        .events(&local)
        .unwrap()
        .append_events(vec![
            wanted.clone(),
            make(Uuid::new_v4(), EventKind::Refusal),
            make(subject, EventKind::Audit),
        ])
        .await
        .unwrap();
    runtime
        .events(&other)
        .unwrap()
        .append_event(make(subject, EventKind::Refusal))
        .await
        .unwrap();
    let listed = pack
        .handle_list(
            &local,
            json!({
                "kind": "event", "event_kind": "refusal", "target_id": subject,
            }),
            &registry,
        )
        .await
        .unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], wanted.id.to_string());
    assert_eq!(items[0]["target_id"], subject.to_string());
    assert_eq!(items[0]["namespace"], "local");
    assert_eq!(items[0]["kind"], "refusal");

    let observed = pack
        .handle_list(
            &local,
            json!({
                "kind": "event", "event_kind": "refusal", "observed": [subject],
            }),
            &registry,
        )
        .await
        .unwrap();
    assert!(observed["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn edge_target_still_resolves_primary_namespace_prefix_and_name() {
    let (runtime, pack, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let source = runtime
        .create_entity(&token, "concept", None, "source", None, None, vec![])
        .await
        .unwrap();
    let target = runtime
        .create_entity(&token, "concept", None, "target-name", None, None, vec![])
        .await
        .unwrap();
    pack.handle_link(
        &token,
        json!({
            "source_id": source.id, "target_id": target.id, "relation": "supports",
        }),
        &registry,
    )
    .await
    .unwrap();
    for raw in [
        target.id.to_string(),
        target.id.simple().to_string()[..8].to_owned(),
        "target-name".into(),
    ] {
        let result = pack
            .handle_list(&token, json!({"kind": "edge", "target_id": raw}), &registry)
            .await
            .unwrap();
        let edges = result["items"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["target_id"], target.id.to_string());
    }
}

#[test]
fn list_target_help_and_schema_describe_both_identifier_contracts() {
    let (_, _, registry) = fixture();
    let help = registry.describe_verb("list").unwrap();
    let target = help["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "target_id")
        .unwrap();
    let description = target["description"].as_str().unwrap();
    assert!(description.contains("kind=event accepts only a full subject UUID"));
    assert!(description.contains("prefixes and names are rejected without graph resolution"));
    assert!(description.contains("For kind=edge"));
    assert_eq!(
        help["input_schema"]["properties"]["target_id"]["description"],
        Value::String(description.into())
    );
}
