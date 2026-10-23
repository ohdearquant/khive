//! A retired registry name cannot be revived through the ordinary registration surface.

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder};
use khive_storage::Entity;
use serde_json::json;

#[tokio::test]
async fn retired_registry_name_requires_registering_a_new_name() {
    let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    let registry = builder.build().expect("tool registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());

    let token = runtime.authorize(Namespace::local()).unwrap();
    let entities = runtime.entities(&token).unwrap();
    let mut tombstone = Entity::new("local", "project", "retired_tool");
    tombstone.entity_type = Some("tool".into());
    tombstone.tags = vec!["tool-registry".into(), "tool".into()];
    tombstone.properties = Some(json!({"source": "fixture:original", "trust": "first_party"}));
    tombstone.deleted_at = Some(tombstone.updated_at);
    let id = tombstone.id;
    // Public delete refuses registry rows; direct storage represents a historical tombstone.
    entities.upsert_entity(tombstone).await.unwrap();
    let before = entities
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .expect("seeded tombstone exists");
    assert!(before.deleted_at.is_some());

    let error = registry
        .dispatch("tool.register", json!({"name": "retired_tool"}))
        .await
        .expect_err("a retired name must stay retired");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("expected retired-name refusal, got {error:?}");
    };
    assert_eq!(
        message,
        format!(
            "registry name \"retired_tool\" is soft-deleted at entity {id}; the name stays retired; register a new name"
        )
    );
    assert!(
        !message.contains("restore"),
        "obsolete recovery advice: {message}"
    );
    let after = entities
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .expect("refused tombstone remains stored");
    assert!(after.deleted_at.is_some());
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap()
    );

    let replacement = registry
        .dispatch("tool.register", json!({"name": "replacement_tool"}))
        .await
        .expect("the suggested new-name path succeeds");
    assert_eq!(replacement["created"], true);
    assert_eq!(replacement["tool"]["name"], "replacement_tool");
    assert_ne!(replacement["tool"]["full_id"], id.to_string());
    let described = registry
        .dispatch("tool.describe", json!({"tool": "replacement_tool"}))
        .await
        .expect("the new registry name is usable");
    assert_eq!(described["tool"]["name"], "replacement_tool");
    let listed = registry.dispatch("tool.list", json!({})).await.unwrap();
    assert_eq!(listed["count"], 1);
    assert!(entities
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
}
