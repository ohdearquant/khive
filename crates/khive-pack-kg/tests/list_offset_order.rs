//! `list` offset pages return records newest first. Entities break a
//! `created_at` tie by `id` descending; notes break it by `id` ascending.
//! The API reference states both orders, and these tests pin them at the verb.

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{Entity, Note};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let actor = "list-offset-order-test";
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        actor_id: Some(actor.to_string()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime without embeddings");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.to_string()));
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

/// Three rows: two share the newer timestamp, and the older one has the
/// highest id, so neither id order alone reproduces the expected page.
const ROWS: [(u128, i64); 3] = [(3, 1_000_000), (1, 2_000_000), (2, 2_000_000)];

async fn listed_ids(registry: &VerbRegistry, kind: &str) -> Vec<String> {
    let response: Value = registry
        .dispatch("list", json!({"kind": kind, "limit": 10}))
        .await
        .expect("list dispatch");
    response["items"]
        .as_array()
        .expect("offset page items")
        .iter()
        .map(|item| item["id"].as_str().expect("item id").to_string())
        .collect()
}

fn ids(order: [u128; 3]) -> Vec<String> {
    order
        .iter()
        .map(|id| Uuid::from_u128(*id).to_string())
        .collect()
}

#[tokio::test]
async fn list_entities_offset_page_is_newest_first_with_id_descending_tiebreak() {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    for (id, timestamp) in ROWS {
        let mut entity = Entity::new("local", "concept", format!("order-{id}"));
        entity.id = Uuid::from_u128(id);
        entity.created_at = timestamp;
        entity.updated_at = timestamp;
        runtime
            .entities(&token)
            .unwrap()
            .upsert_entity(entity)
            .await
            .unwrap();
    }
    assert_eq!(listed_ids(&registry, "entity").await, ids([2, 1, 3]));
}

#[tokio::test]
async fn list_notes_offset_page_is_newest_first_with_id_ascending_tiebreak() {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    for (id, timestamp) in ROWS {
        let mut note = Note::new("local", "observation", format!("order-{id}"));
        note.id = Uuid::from_u128(id);
        note.created_at = timestamp;
        note.updated_at = timestamp;
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
    }
    assert_eq!(listed_ids(&registry, "note").await, ids([1, 2, 3]));
}
