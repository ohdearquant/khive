//! Registry identities remain retired even when a tombstone predates their write guard.

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::Entity;
use serde_json::json;

fn surface() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

async fn seed_tombstone(runtime: &KhiveRuntime, namespace: &str, tags: &[&str]) -> Entity {
    let token = runtime
        .authorize(Namespace::parse(namespace).expect("fixture namespace"))
        .expect("fixture authorization");
    let mut entity = Entity::new(namespace, "project", "retired registry fixture");
    entity.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
    entity.properties = Some(json!({"source": "fixture:original", "trust": "first_party"}));
    entity.deleted_at = Some(entity.updated_at);
    let id = entity.id;
    let entities = runtime.entities(&token).expect("entity store");
    // Public delete already refuses registry rows. Seed a historical tombstone directly
    // through storage so this test exercises public restore rather than that delete guard.
    entities
        .upsert_entity(entity)
        .await
        .expect("seed tombstone");
    let stored = entities
        .get_entity_including_deleted(id)
        .await
        .expect("read seeded tombstone")
        .expect("seeded row exists");
    assert!(stored.deleted_at.is_some());
    stored
}

async fn assert_unchanged(runtime: &KhiveRuntime, before: &Entity) {
    let token = runtime
        .authorize(Namespace::parse(&before.namespace).expect("fixture namespace"))
        .expect("fixture authorization");
    let after = runtime
        .entities(&token)
        .expect("entity store")
        .get_entity_including_deleted(before.id)
        .await
        .expect("read refused tombstone")
        .expect("refused row remains stored");
    assert!(
        after.deleted_at.is_some(),
        "refused restore revived the row"
    );
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap(),
        "refusal must preserve the complete stored tombstone"
    );
}

async fn assert_registry_restore_refused(tag: &str) {
    let (runtime, registry) = surface();
    let tombstone = seed_tombstone(&runtime, "local", &[tag]).await;
    let error = registry
        .dispatch("restore", json!({"id": tombstone.id, "kind": "entity"}))
        .await
        .expect_err("registry tombstones must not be restored");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("expected registry refusal, got {error:?}");
    };
    assert!(message.contains("restore refuses"), "{message}");
    assert!(message.contains("tool-registry"), "{message}");
    assert!(message.contains("registering a new name"), "{message}");
    assert_unchanged(&runtime, &tombstone).await;
}

#[tokio::test]
async fn restore_refuses_tool_registry_tombstone() {
    assert_registry_restore_refused("tool-registry").await;
}

#[tokio::test]
async fn restore_refuses_case_variant_registry_tag() {
    assert_registry_restore_refused("ToOl-ReGiStRy").await;
}

#[tokio::test]
async fn restore_keeps_untagged_entity_behavior() {
    let (runtime, registry) = surface();
    let tombstone = seed_tombstone(&runtime, "local", &[]).await;
    let response = registry
        .dispatch("restore", json!({"id": tombstone.id, "kind": "entity"}))
        .await
        .expect("ordinary entity tombstone restores");
    assert_eq!(response["restored"], true);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let restored = runtime
        .entities(&token)
        .unwrap()
        .get_entity_including_deleted(tombstone.id)
        .await
        .unwrap()
        .expect("restored entity remains stored");
    assert!(restored.deleted_at.is_none());
    assert_eq!(restored.id, tombstone.id);
    assert_eq!(restored.namespace, tombstone.namespace);
    assert_eq!(restored.name, tombstone.name);
    assert_eq!(restored.tags, tombstone.tags);
    assert_eq!(restored.properties, tombstone.properties);

    let again = registry
        .dispatch("restore", json!({"id": tombstone.id, "kind": "entity"}))
        .await
        .expect("live ordinary entity remains an idempotent restore");
    assert_eq!(again["restored"], false);
}

#[tokio::test]
async fn foreign_registry_tombstone_does_not_disclose_registry_tag() {
    let (runtime, registry) = surface();
    let tombstone = seed_tombstone(&runtime, "foreign", &["tool-registry"]).await;
    for kind in [Some("entity"), Some("concept"), None] {
        let mut params = json!({"id": tombstone.id});
        if let Some(kind) = kind {
            params["kind"] = json!(kind);
        }
        let error = registry
            .dispatch("restore", params)
            .await
            .expect_err("foreign restore must refuse before registry inspection");
        let RuntimeError::NotFound(message) = error else {
            panic!("expected ownership refusal, got {error:?}");
        };
        assert_eq!(
            message,
            format!(
                "restore requires a tombstone in the caller's namespace: {}",
                tombstone.id
            )
        );
        assert!(!message.contains("tool-registry"));
        assert_unchanged(&runtime, &tombstone).await;
    }
}
