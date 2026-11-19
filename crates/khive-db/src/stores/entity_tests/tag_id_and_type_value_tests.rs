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
