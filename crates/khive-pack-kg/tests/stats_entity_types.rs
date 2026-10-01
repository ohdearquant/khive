//! Entity type counts through the public stats dispatch path.

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RequestIdentity, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{DeleteMode, Entity};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

async fn seed(
    runtime: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    entity_type: Option<&str>,
) -> Uuid {
    let token = runtime
        .authorize(Namespace::parse(namespace).expect("namespace"))
        .expect("token");
    let entity = Entity::new(namespace, kind, "StatsTypeFixture").with_entity_type(entity_type);
    let id = entity.id;
    runtime
        .entities(&token)
        .expect("entity store")
        .upsert_entity(entity)
        .await
        .expect("raw historical entity");
    id
}

async fn stats(registry: &VerbRegistry, visible: &[&str]) -> Value {
    registry
        .dispatch_with_identity(
            "stats",
            json!({}),
            Some(RequestIdentity {
                namespace: "local".into(),
                actor_id: None,
                visible_namespaces: visible
                    .iter()
                    .map(|namespace| (*namespace).into())
                    .collect(),
                ..RequestIdentity::default()
            }),
        )
        .await
        .expect("public stats")
}

fn assert_entity_counts(result: &Value, entities: u64, groups: Value) {
    assert_eq!(
        result["count_scope"],
        json!({"namespaces":"caller_visible", "rows":"live_only"})
    );
    assert_eq!(result["entities"], json!(entities));
    assert_eq!(result["entities_by_type"], groups);
    let buckets = result["entities_by_type"]
        .as_array()
        .expect("type count array");
    assert_eq!(
        buckets
            .iter()
            .map(|bucket| bucket["count"].as_u64().expect("integer count"))
            .sum::<u64>(),
        entities
    );
    assert_eq!(
        buckets
            .iter()
            .filter(|bucket| bucket["entity_type"].is_null())
            .count(),
        1
    );
    assert!(buckets[0]["entity_type"].is_null());
}

#[tokio::test]
async fn stats_entity_types_count_only_live_rows_in_each_callers_visible_namespaces() {
    let (runtime, registry) = fixture();
    for (namespace, entity_type, count) in [
        ("local", None, 2),
        ("local", Some("algorithm"), 1),
        ("lambda:stats-alpha", None, 1),
        ("lambda:stats-alpha", Some("algorithm"), 2),
        ("lambda:stats-alpha", Some("model"), 1),
        ("lambda:stats-beta", Some("benchmark"), 3),
        ("lambda:stats-hidden", Some("hidden"), 4),
        ("lambda:stats-hidden", None, 1),
    ] {
        for _ in 0..count {
            seed(&runtime, namespace, "concept", entity_type).await;
        }
    }
    for (namespace, entity_type) in [
        ("local", None),
        ("lambda:stats-alpha", Some("algorithm")),
        ("lambda:stats-beta", Some("deleted-only")),
    ] {
        let id = seed(&runtime, namespace, "concept", entity_type).await;
        let token = runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        assert!(runtime
            .entities(&token)
            .unwrap()
            .delete_entity(id, DeleteMode::Soft)
            .await
            .unwrap());
    }
    let alpha = stats(&registry, &["lambda:stats-alpha"]).await;
    assert_entity_counts(
        &alpha,
        7,
        json!([
            {"entity_type":null,"count":3},
            {"entity_type":"algorithm","count":3},
            {"entity_type":"model","count":1}
        ]),
    );
    let beta = stats(&registry, &["lambda:stats-beta"]).await;
    assert_entity_counts(
        &beta,
        6,
        json!([
            {"entity_type":null,"count":2},
            {"entity_type":"algorithm","count":1},
            {"entity_type":"benchmark","count":3}
        ]),
    );
    assert_entity_counts(
        &stats(&registry, &[]).await,
        3,
        json!([
            {"entity_type":null,"count":2},
            {"entity_type":"algorithm","count":1}
        ]),
    );
}

#[tokio::test]
async fn stats_entity_types_keep_null_and_raw_labels_distinct_in_byte_order() {
    let (runtime, registry) = fixture();
    for entity_type in [
        None,
        None,
        Some("null"),
        Some("null"),
        Some("<null>"),
        Some("<null>"),
        Some(""),
        Some("\\<null>"),
        Some("Z"),
        Some("alpha"),
        Some("Ä"),
    ] {
        seed(&runtime, "local", "concept", entity_type).await;
    }
    seed(&runtime, "local", "document", Some("<null>")).await;
    assert_entity_counts(
        &stats(&registry, &[]).await,
        12,
        json!([
            {"entity_type":null,"count":2},
            {"entity_type":"","count":1},
            {"entity_type":"<null>","count":3},
            {"entity_type":"Z","count":1},
            {"entity_type":"\\<null>","count":1},
            {"entity_type":"alpha","count":1},
            {"entity_type":"null","count":2},
            {"entity_type":"Ä","count":1}
        ]),
    );
}

#[tokio::test]
async fn stats_entity_types_include_zero_null_bucket_for_empty_and_typed_only_stores() {
    let (runtime, registry) = fixture();
    assert_entity_counts(
        &stats(&registry, &[]).await,
        0,
        json!([{"entity_type":null,"count":0}]),
    );
    seed(&runtime, "local", "concept", Some("algorithm")).await;
    assert_entity_counts(
        &stats(&registry, &[]).await,
        1,
        json!([
            {"entity_type":null,"count":0},
            {"entity_type":"algorithm","count":1}
        ]),
    );
}
