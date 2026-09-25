use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Event, EventOutcome};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

const ACTOR: &str = "actor:list-filter-scope";

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        actor_id: Some(ACTOR.into()),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn items(value: &Value) -> &[Value] {
    value["items"].as_array().unwrap()
}

#[tokio::test]
async fn list_rejects_filters_for_the_wrong_kind_even_when_empty_or_null() {
    let (_, registry) = fixture();
    let created = registry
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "visible observation"}),
        )
        .await
        .unwrap();
    let before = registry
        .dispatch("list", json!({"kind": "observation"}))
        .await
        .unwrap();
    assert_eq!(items(&before).len(), 1);

    for (kind, field, value) in [
        ("observation", "actor", json!(ACTOR)),
        ("observation", "target_id", json!(Uuid::nil())),
        ("concept", "source_id", json!(Uuid::nil())),
        ("note", "relations", json!([])),
        ("note", "min_weight", json!(0)),
        ("concept", "max_weight", json!(1)),
        ("edge", "entity_kind", json!("concept")),
        ("note", "entity_type", json!("paper")),
        ("concept", "note_kind", json!("observation")),
        ("event", "tags", json!([])),
        ("edge", "tags", json!([])),
        ("entity", "key_prefix", json!("")),
        ("event", "after_key", json!("")),
        ("entity", "created_after", json!("2026-09-24T00:00:00Z")),
        ("edge", "updated_after", json!("2026-09-24T00:00:00Z")),
        ("event", "tag_mode", json!("any")),
        ("entity", "thread_id", json!("legacy-thread")),
        ("edge", "direction", json!("inbound")),
        ("event", "from", json!("sender")),
        ("entity", "to", json!("recipient")),
        ("edge", "read", json!(false)),
        ("event", "delivered", json!(false)),
        ("note", "verb", json!("create")),
        ("entity", "verbs", json!([])),
        ("edge", "outcome", json!("success")),
        ("note", "substrate", json!("entity")),
        ("entity", "since", json!(0)),
        ("edge", "until", json!(0)),
        ("note", "event_kind", json!("audit")),
        ("entity", "event_kinds", json!([])),
        ("edge", "session_id", json!(Uuid::nil())),
        ("note", "observed", json!([])),
        ("entity", "selected", json!([])),
        ("event", "after", json!("")),
        ("event", "status", json!("pending")),
        ("entity", "created_by_actor", json!(ACTOR)),
        ("proposal", "target_id", json!(Uuid::nil())),
        ("proposal", "verbs", json!([])),
        ("proposal", "tags", json!([])),
    ] {
        for sent in [value, Value::Null] {
            let mut args = json!({"kind": kind});
            args[field] = sent;
            let error = registry.dispatch("list", args).await.unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
            let message = error.to_string();
            assert!(message.contains(field), "{kind}/{field}: {message}");
            assert!(
                message.contains(&format!("kind={kind:?}")),
                "{kind}/{field}: {message}"
            );
        }
    }

    let after = registry
        .dispatch("list", json!({"kind": "observation"}))
        .await
        .unwrap();
    assert_eq!(before, after);
    let record = registry
        .dispatch("get", json!({"id": created["id"]}))
        .await
        .unwrap();
    assert_eq!(record["content"], "visible observation");
}

#[tokio::test]
async fn list_rejects_unknown_parameters_by_name_on_every_substrate() {
    let (_, registry) = fixture();
    for kind in ["concept", "observation", "edge", "event", "proposal"] {
        for value in [json!("sentinel"), Value::Null] {
            let error = registry
                .dispatch("list", json!({"kind": kind, "not_a_list_parameter": value}))
                .await
                .unwrap_err();
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
            assert!(
                error.to_string().contains("not_a_list_parameter"),
                "{error}"
            );
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }
}

#[tokio::test]
async fn list_event_target_and_actor_filter_before_pagination_and_return_empty_on_miss() {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let stored_actor = format!("{}:{}", token.actor().kind, token.actor().id);
    let other = runtime
        .authorize(Namespace::parse("other").unwrap())
        .unwrap();
    let target = Uuid::new_v4();
    let make = |target, timestamp, verb, outcome| {
        let mut event = Event::new(
            "ignored",
            verb,
            EventKind::Audit,
            SubstrateKind::Note,
            "ignored",
        )
        .with_outcome(outcome);
        event.target_id = target;
        event.created_at = timestamp;
        event
    };
    let older = make(Some(target), 1_000, "fixture.write", EventOutcome::Denied);
    let newer = make(Some(target), 3_000, "fixture.write", EventOutcome::Success);
    runtime
        .events(&token)
        .unwrap()
        .append_events(vec![
            older.clone(),
            make(Some(target), 2_000, "another.write", EventOutcome::Success),
            newer.clone(),
            make(
                Some(Uuid::new_v4()),
                4_000,
                "fixture.write",
                EventOutcome::Success,
            ),
            make(None, 6_000, "fixture.write", EventOutcome::Success),
        ])
        .await
        .unwrap();
    runtime
        .events(&other)
        .unwrap()
        .append_event(make(
            Some(target),
            5_000,
            "fixture.write",
            EventOutcome::Success,
        ))
        .await
        .unwrap();

    let query = json!({"kind": "event", "target_id": target, "actor": stored_actor, "verb": "fixture.write", "limit": 1});
    for (offset, wanted, has_more) in [(0, &newer, true), (1, &older, false)] {
        let mut args = query.clone();
        args["offset"] = json!(offset);
        let page = registry.dispatch("list", args).await.unwrap();
        assert_eq!(items(&page).len(), 1);
        assert_eq!(items(&page)[0]["id"], wanted.id.to_string());
        assert_eq!(items(&page)[0]["target_id"], target.to_string());
        assert_eq!(items(&page)[0]["actor"], stored_actor);
        assert_eq!(items(&page)[0]["namespace"], "local");
        assert_eq!(page["has_more"], has_more);
    }
    for (field, value) in [
        ("offset", json!(2)),
        ("target_id", json!(Uuid::nil())),
        ("actor", json!("actor:no-match")),
    ] {
        let mut args = query.clone();
        args[field] = value;
        let page = registry.dispatch("list", args).await.unwrap();
        assert!(items(&page).is_empty(), "{field}: {page}");
        assert_eq!(page["has_more"], false);
    }
    let mut denied = query.clone();
    denied["outcome"] = json!("denied");
    let page = registry.dispatch("list", denied).await.unwrap();
    assert_eq!(items(&page).len(), 1);
    assert_eq!(items(&page)[0]["id"], older.id.to_string());

    let unfiltered = registry
        .dispatch("list", json!({"kind": "event", "verb": "fixture.write"}))
        .await
        .unwrap();
    assert_eq!(items(&unfiltered).len(), 4);
}

#[tokio::test]
async fn list_keeps_applicable_note_entity_edge_and_proposal_filters() {
    let (_, registry) = fixture();
    let mut entities = Vec::new();
    for (name, tag) in [("first", "wanted"), ("second", "other")] {
        entities.push(
            registry
                .dispatch(
                    "create",
                    json!({"kind": "concept", "name": name, "tags": [tag]}),
                )
                .await
                .unwrap(),
        );
        registry
            .dispatch(
                "create",
                json!({"kind": "observation", "content": name, "tags": [tag]}),
            )
            .await
            .unwrap();
    }
    for kind in ["entity", "concept", "note", "observation"] {
        let page = registry
            .dispatch("list", json!({"kind": kind, "tags": ["wanted"]}))
            .await
            .unwrap();
        assert_eq!(items(&page).len(), 1, "{kind}: {page}");
    }
    registry.dispatch("link", json!({"source_id": entities[0]["id"], "target_id": entities[1]["id"], "relation": "supports"})).await.unwrap();
    let page = registry
        .dispatch(
            "list",
            json!({"kind": "edge", "target_id": entities[1]["id"], "relations": ["supports"]}),
        )
        .await
        .unwrap();
    assert_eq!(items(&page).len(), 1);
    assert_eq!(items(&page)[0]["source_id"], entities[0]["id"]);
    let proposals = registry
        .dispatch(
            "list",
            json!({"kind": "proposal", "actor": "*", "status": "open"}),
        )
        .await
        .unwrap();
    assert!(items(&proposals).is_empty());
}

#[test]
fn list_help_and_schema_declare_supported_event_and_proposal_filters() {
    let (_, registry) = fixture();
    let help = registry.describe_verb("list").unwrap();
    for name in [
        "target_id",
        "verb",
        "verbs",
        "outcome",
        "actor",
        "proposer",
        "substrate",
        "since",
        "until",
    ] {
        let param = help["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(
            help["input_schema"]["properties"][name]["description"],
            param["description"]
        );
    }
    let actor = help["input_schema"]["properties"]["actor"]["description"]
        .as_str()
        .unwrap();
    assert!(actor.contains("observation"));
    assert!(actor.contains("proposals"));
}
