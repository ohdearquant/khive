//! Keyed entity create and lookup through the kg verbs (ADR-221 section 1).

use khive_runtime::RuntimeError;
use serde_json::{json, Value};

async fn configured() -> (
    khive_runtime::KhiveRuntime,
    khive_runtime::NamespaceToken,
    crate::KgPack,
    khive_runtime::VerbRegistry,
) {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let pack = KgPack::new(rt.clone());
    (rt, token, pack, registry)
}

fn details(error: &RuntimeError) -> (Option<String>, Option<String>) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected a domain error, got {error:?}");
    };
    let details = error.details().expect("details");
    (
        details.get("reason").map(str::to_owned),
        details.get("existing_id").map(str::to_owned),
    )
}

fn keyed_create(name: &str) -> Value {
    json!({
        "kind": "concept",
        "name": name,
        "key": "paper:smith2024",
        "skip_dedup_check": true
    })
}

#[tokio::test]
async fn keyed_entity_create_replays_and_refuses_a_different_payload() {
    let (_rt, _token, _pack, registry) = configured().await;
    let first = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("fresh keyed create");
    assert_eq!(first["created"], json!(true), "{first}");
    let id = first["id"].as_str().expect("id").to_owned();

    let replay = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("replay");
    assert_eq!(replay, json!({"id": id, "created": false}));

    let error = registry
        .dispatch("create", keyed_create("Someone else"))
        .await
        .expect_err("a different payload under a held key");
    assert_eq!(
        details(&error),
        (Some("key_conflict".to_owned()), Some(id.clone()))
    );

    let listed = registry
        .dispatch("list", json!({"kind": "concept"}))
        .await
        .expect("list");
    let count = listed
        .get("items")
        .or_else(|| listed.get("results"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_else(|| listed.as_array().map(Vec::len).unwrap_or_default());
    assert_eq!(count, 1, "replay and conflict write nothing: {listed}");
}

#[tokio::test]
async fn get_by_entity_key_reads_the_live_holder() {
    let (_rt, _token, _pack, registry) = configured().await;
    let created = registry
        .dispatch("create", keyed_create("Smith 2024"))
        .await
        .expect("keyed create");
    let got = registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
        .await
        .expect("get by key");
    assert_eq!(got["id"], created["id"], "{got}");
    assert_eq!(got["name"], json!("Smith 2024"));

    registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:absent"}))
        .await
        .expect_err("no holder");
    registry
        .dispatch("get", json!({"kind": "project", "key": "paper:smith2024"}))
        .await
        .expect_err("the key is scoped to its kind");
    let error = registry
        .dispatch("get", json!({"kind": "entity", "key": "paper:smith2024"}))
        .await
        .expect_err("an entity key lookup needs a specific kind");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

#[tokio::test]
async fn key_is_refused_on_a_substrate_without_keys() {
    let (_rt, _token, _pack, registry) = configured().await;
    let a = registry
        .dispatch("create", json!({"kind": "concept", "name": "A"}))
        .await
        .unwrap();
    let b = registry
        .dispatch("create", json!({"kind": "concept", "name": "B"}))
        .await
        .unwrap();
    let error = registry
        .dispatch(
            "create",
            json!({
                "kind": "edge",
                "source_id": a["id"],
                "target_id": b["id"],
                "relation": "extends",
                "key": "k"
            }),
        )
        .await
        .expect_err("edges hold no key");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

#[tokio::test]
async fn delete_releases_and_restore_refuses_a_taken_entity_key() {
    let (_rt, token, pack, registry) = configured().await;
    let first = registry
        .dispatch("create", keyed_create("First"))
        .await
        .expect("keyed create");
    let first_id = first["id"].as_str().unwrap().to_owned();
    registry
        .dispatch("delete", json!({"id": first_id}))
        .await
        .expect("soft delete");
    registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
        .await
        .expect_err("a deleted entity holds no key");

    let second = registry
        .dispatch("create", keyed_create("Second"))
        .await
        .expect("the released key is free");
    assert_eq!(second["created"], json!(true));
    let second_id = second["id"].as_str().unwrap().to_owned();
    assert_ne!(second_id, first_id);

    let conflict = pack
        .handle_restore(&token, json!({"id": first_id}), &registry)
        .await
        .expect_err("restore must refuse a taken key");
    let text = format!("{conflict}");
    assert!(text.contains("restore_key_conflict"), "{text}");
    assert!(text.contains("paper:smith2024"), "{text}");
    assert!(text.contains(&second_id), "{text}");

    let tombstone = registry
        .dispatch("get", json!({"id": first_id, "include_deleted": true}))
        .await
        .expect("the refused restore leaves the tombstone");
    assert!(tombstone["deleted_at"].is_string(), "{tombstone}");
    let holder = registry
        .dispatch("get", json!({"kind": "concept", "key": "paper:smith2024"}))
        .await
        .expect("the second entity still holds the key");
    assert_eq!(holder["id"], json!(second_id));
}
