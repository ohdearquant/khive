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
