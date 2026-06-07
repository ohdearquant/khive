use super::*;
use crate::pool::PoolConfig;

fn setup_pool() -> Arc<ConnectionPool> {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(ENTITIES_DDL).unwrap();
    }
    pool
}

fn setup_memory_store() -> SqlEntityStore {
    SqlEntityStore::new(setup_pool(), false)
}

fn setup_memory_store_ns(_ns: &str) -> SqlEntityStore {
    SqlEntityStore::new(setup_pool(), false)
}

fn make_entity(namespace: &str, kind: &str, name: &str) -> Entity {
    let now = chrono::Utc::now().timestamp_micros();
    Entity {
        id: Uuid::new_v4(),
        namespace: namespace.to_string(),
        kind: kind.to_string(),
        entity_type: None,
        name: name.to_string(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
        merged_into: None,
        merge_event_id: None,
    }
}

#[tokio::test]
async fn test_upsert_and_get_entity() {
    let store = setup_memory_store();

    let entity = make_entity("default", "concept", "LoRA");
    let id = entity.id;

    store.upsert_entity(entity).await.unwrap();

    let fetched = store.get_entity(id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.name, "LoRA");
    assert_eq!(fetched.kind, "concept");
}

#[tokio::test]
async fn test_upsert_with_builder() {
    let store = setup_memory_store();

    let props = serde_json::json!({"domain": "fine-tuning", "type": "technique"});
    let entity = Entity::new("default", "concept", "QLoRA")
        .with_description("Quantized LoRA")
        .with_properties(props.clone())
        .with_tags(vec!["fine-tuning".to_string(), "quantization".to_string()]);
    let id = entity.id;

    store.upsert_entity(entity).await.unwrap();

    let fetched = store.get_entity(id).await.unwrap().unwrap();
    assert_eq!(fetched.description.as_deref(), Some("Quantized LoRA"));
    assert_eq!(fetched.properties, Some(props));
    assert_eq!(fetched.tags, vec!["fine-tuning", "quantization"]);
}

#[tokio::test]
async fn test_soft_delete() {
    let store = setup_memory_store();

    let entity = make_entity("default", "concept", "to-delete");
    let id = entity.id;
    store.upsert_entity(entity).await.unwrap();

    let deleted = store.delete_entity(id, DeleteMode::Soft).await.unwrap();
    assert!(deleted);

    let fetched = store.get_entity(id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn test_hard_delete() {
    let store = setup_memory_store();

    let entity = make_entity("default", "concept", "to-hard-delete");
    let id = entity.id;
    store.upsert_entity(entity).await.unwrap();

    let deleted = store.delete_entity(id, DeleteMode::Hard).await.unwrap();
    assert!(deleted);

    let fetched = store.get_entity(id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn test_query_entities_basic() {
    let store = setup_memory_store_ns("ns1");

    for name in &["Alpha", "Beta", "Gamma"] {
        store
            .upsert_entity(make_entity("ns1", "concept", name))
            .await
            .unwrap();
    }
    store
        .upsert_entity(make_entity("ns1", "document", "Paper1"))
        .await
        .unwrap();

    let page = store
        .query_entities(
            "ns1",
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.total, Some(4));

    // Filter by kind
    let concepts = store
        .query_entities(
            "ns1",
            EntityFilter {
                kinds: vec!["concept".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(concepts.items.len(), 3);
}

#[tokio::test]
async fn test_query_by_name_prefix() {
    let store = setup_memory_store_ns("ns1");

    // "Alpha" and "AlphaGo" both start with "Alpha"; "Beta" does not
    for &name in &["Alpha", "AlphaGo", "Beta"] {
        store
            .upsert_entity(make_entity("ns1", "concept", name))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("Alpha".to_string()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Alpha"), "Alpha not found in {names:?}");
    assert!(names.contains(&"AlphaGo"), "AlphaGo not found in {names:?}");
    assert!(!names.contains(&"Beta"));
}

#[tokio::test]
async fn test_count_entities() {
    let store = setup_memory_store_ns("ns1");

    for _ in 0..5 {
        store
            .upsert_entity(make_entity("ns1", "concept", "X"))
            .await
            .unwrap();
    }

    let count = store
        .count_entities("ns1", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 5);

    // Namespace is the caller's responsibility — querying "ns2" returns 0
    // because no entities were inserted in that namespace.
    let count_other = store
        .count_entities("ns2", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count_other, 0);
}

#[tokio::test]
async fn test_batch_upsert() {
    let store = setup_memory_store_ns("batch_ns");

    let entities: Vec<Entity> = (0..10)
        .map(|i| make_entity("batch_ns", "concept", &format!("entity_{i}")))
        .collect();

    let summary = store.upsert_entities(entities).await.unwrap();
    assert_eq!(summary.attempted, 10);
    assert_eq!(summary.affected, 10);
    assert_eq!(summary.failed, 0);

    let count = store
        .count_entities("batch_ns", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 10);
}

/// One store, two namespaces — each query sees only its own.
#[tokio::test]
async fn test_namespace_isolation() {
    let pool = setup_pool();
    let store = SqlEntityStore::new(Arc::clone(&pool), false);

    store
        .upsert_entity(make_entity("ns_a", "concept", "EntityA"))
        .await
        .unwrap();
    store
        .upsert_entity(make_entity("ns_b", "concept", "EntityB"))
        .await
        .unwrap();

    // Namespace is the caller's responsibility — pass it in the query.
    let count_a = store
        .count_entities("ns_a", EntityFilter::default())
        .await
        .unwrap();
    let count_b = store
        .count_entities("ns_b", EntityFilter::default())
        .await
        .unwrap();

    assert_eq!(count_a, 1);
    assert_eq!(count_b, 1);

    let page_a = store
        .query_entities("ns_a", EntityFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_a.items[0].name, "EntityA");

    let page_b = store
        .query_entities("ns_b", EntityFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_b.items[0].name, "EntityB");
}

#[tokio::test]
async fn test_query_by_tags() {
    let store = setup_memory_store_ns("tags_ns");

    let mut e1 = make_entity("tags_ns", "concept", "Tagged1");
    e1.tags = vec!["rust".to_string(), "systems".to_string()];
    let mut e2 = make_entity("tags_ns", "concept", "Tagged2");
    e2.tags = vec!["python".to_string(), "ml".to_string()];
    let mut e3 = make_entity("tags_ns", "concept", "Tagged3");
    e3.tags = vec!["rust".to_string(), "ml".to_string()];

    store.upsert_entity(e1).await.unwrap();
    store.upsert_entity(e2).await.unwrap();
    store.upsert_entity(e3).await.unwrap();

    // Filter by "rust" tag — should match Tagged1 and Tagged3
    let result = store
        .query_entities(
            "tags_ns",
            EntityFilter {
                tags_any: vec!["rust".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Tagged1"));
    assert!(names.contains(&"Tagged3"));
    assert!(!names.contains(&"Tagged2"));

    // Filter by "ml" tag — should match Tagged2 and Tagged3
    let result = store
        .query_entities(
            "tags_ns",
            EntityFilter {
                tags_any: vec!["ml".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);

    // Filter by both "rust" and "python" (union) — should match all three
    let result = store
        .query_entities(
            "tags_ns",
            EntityFilter {
                tags_any: vec!["rust".to_string(), "python".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 3);
}

#[tokio::test]
async fn test_query_by_ids() {
    let store = setup_memory_store_ns("ns1");

    let e1 = make_entity("ns1", "concept", "E1");
    let e2 = make_entity("ns1", "concept", "E2");
    let e3 = make_entity("ns1", "concept", "E3");
    let ids = vec![e1.id, e3.id];

    store.upsert_entity(e1).await.unwrap();
    store.upsert_entity(e2).await.unwrap();
    store.upsert_entity(e3).await.unwrap();

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                ids,
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"E1"));
    assert!(names.contains(&"E3"));
    assert!(!names.contains(&"E2"));
}

#[tokio::test]
async fn test_entity_type_roundtrip() {
    let store = setup_memory_store();

    let entity =
        Entity::new("default", "document", "ResearchPaper").with_entity_type(Some("paper"));
    let id = entity.id;

    store.upsert_entity(entity).await.unwrap();

    let fetched = store.get_entity(id).await.unwrap().unwrap();
    assert_eq!(fetched.entity_type, Some("paper".to_string()));
    assert_eq!(fetched.kind, "document");
    assert_eq!(fetched.name, "ResearchPaper");
}

#[tokio::test]
async fn test_query_by_kind_and_entity_type() {
    let store = setup_memory_store_ns("et_ns");

    let typed = Entity::new("et_ns", "person", "Researcher").with_entity_type(Some("researcher"));
    let untyped = make_entity("et_ns", "person", "Generic");

    store.upsert_entity(typed).await.unwrap();
    store.upsert_entity(untyped).await.unwrap();

    let result = store
        .query_entities(
            "et_ns",
            EntityFilter {
                entity_types: vec!["researcher".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].name, "Researcher");
    assert_eq!(result.items[0].entity_type, Some("researcher".to_string()));
}

/// UUID is globally unique (id TEXT PRIMARY KEY). Upserting the same UUID in a
/// different namespace overwrites the row (INSERT OR REPLACE). get_entity by ID
/// returns whichever namespace currently owns that UUID.
#[tokio::test]
async fn test_same_id_upsert_replaces_row() {
    let pool = setup_pool();
    let store = SqlEntityStore::new(Arc::clone(&pool), false);

    let shared_id = Uuid::new_v4();
    let now = chrono::Utc::now().timestamp_micros();

    let entity_a = Entity {
        id: shared_id,
        namespace: "ns_a".to_string(),
        kind: "concept".to_string(),
        entity_type: None,
        name: "SharedInA".to_string(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
        merged_into: None,
        merge_event_id: None,
    };
    store.upsert_entity(entity_a).await.unwrap();

    // At this point the row is in ns_a.
    let fetched = store.get_entity(shared_id).await.unwrap().unwrap();
    assert_eq!(fetched.namespace, "ns_a");
    assert_eq!(fetched.name, "SharedInA");

    // Upsert same UUID into ns_b — INSERT OR REPLACE replaces the row.
    let entity_b = Entity {
        id: shared_id,
        namespace: "ns_b".to_string(),
        kind: "concept".to_string(),
        entity_type: None,
        name: "SharedInB".to_string(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
        merged_into: None,
        merge_event_id: None,
    };
    store.upsert_entity(entity_b).await.unwrap();

    // Now the row is in ns_b — get_entity returns ns_b regardless of which namespace
    // you query from (namespace is caller's responsibility).
    let fetched = store.get_entity(shared_id).await.unwrap().unwrap();
    assert_eq!(fetched.namespace, "ns_b");
    assert_eq!(fetched.name, "SharedInB");

    // ns_a now has 0 entities; ns_b has 1.
    let count_a = store
        .count_entities("ns_a", EntityFilter::default())
        .await
        .unwrap();
    let count_b = store
        .count_entities("ns_b", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count_a, 0);
    assert_eq!(count_b, 1);
}
