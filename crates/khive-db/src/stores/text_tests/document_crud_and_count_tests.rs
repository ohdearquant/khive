#[tokio::test]
async fn test_count_with_filter() {
    let store = setup_memory_store("count_filter");
    let ns = "test_ns".to_string();

    for i in 0..5 {
        let kind = if i % 2 == 0 {
            SubstrateKind::Entity
        } else {
            SubstrateKind::Note
        };
        let doc = TextDocument {
            subject_id: Uuid::new_v4(),
            kind,
            record_kind: None,
            title: Some(format!("Doc {}", i)),
            body: format!("Content for document number {}", i),
            tags: vec![],
            namespace: ns.clone(),
            metadata: None,
            updated_at: Utc::now(),
        };
        store.upsert_document(doc).await.unwrap();
    }

    let total = store
        .count(TextFilter {
            namespaces: vec![ns.clone()],
            ..TextFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(total, 5);

    let entities = store
        .count(TextFilter {
            namespaces: vec![ns.clone()],
            kinds: vec![SubstrateKind::Entity],
            ..TextFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(entities, 3);

    let notes = store
        .count(TextFilter {
            namespaces: vec![ns.clone()],
            kinds: vec![SubstrateKind::Note],
            ..TextFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(notes, 2);
}

#[tokio::test]
async fn test_get_document_roundtrip() {
    let store = setup_memory_store("get_roundtrip");

    let id = Uuid::new_v4();
    let original = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Note,
        record_kind: Some("memory".to_string()),
        title: Some("Important Memo".to_string()),
        body: "This memo contains critical information.".to_string(),
        tags: vec!["important".to_string(), "memo".to_string()],
        namespace: "work".to_string(),
        metadata: Some(serde_json::json!({"priority": "high"})),
        updated_at: Utc::now(),
    };

    store.upsert_document(original.clone()).await.unwrap();

    let retrieved = store.get_document("work", id).await.unwrap().unwrap();
    assert_eq!(retrieved.subject_id, id);
    assert_eq!(retrieved.kind, SubstrateKind::Note);
    assert_eq!(retrieved.record_kind.as_deref(), Some("memory"));
    assert_eq!(retrieved.title, Some("Important Memo".to_string()));
    assert_eq!(retrieved.body, "This memo contains critical information.");
    assert_eq!(retrieved.tags, vec!["important", "memo"]);
    assert_eq!(retrieved.namespace, "work");
}

#[tokio::test]
async fn test_upsert_replaces_existing() {
    let store = setup_memory_store("replace");

    let id = Uuid::new_v4();
    store
        .upsert_document(make_document(id, "Original", "Original body text."))
        .await
        .unwrap();

    store
        .upsert_document(make_document(id, "Updated", "Updated body text."))
        .await
        .unwrap();

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.document_count, 1);

    let doc = store.get_document("test_ns", id).await.unwrap().unwrap();
    assert_eq!(doc.title, Some("Updated".to_string()));
    assert_eq!(doc.body, "Updated body text.");
}

#[tokio::test]
async fn test_batch_upsert() {
    let store = setup_memory_store("batch");

    let docs: Vec<TextDocument> = (0..50)
        .map(|i| TextDocument {
            subject_id: Uuid::new_v4(),
            kind: SubstrateKind::Entity,
            record_kind: None,
            title: Some(format!("Item {}", i)),
            body: format!("This is the body content for item number {}", i),
            tags: vec![format!("tag_{}", i % 5)],
            namespace: "batch_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        })
        .collect();

    let summary = store.upsert_documents(docs).await.unwrap();
    assert_eq!(summary.attempted, 50);
    assert_eq!(summary.affected, 50);
    assert_eq!(summary.failed, 0);

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.document_count, 50);
}

#[tokio::test]
async fn test_empty_search() {
    let store = setup_memory_store("empty");

    let hits = store
        .search(TextSearchRequest {
            query: "nonexistent".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert!(hits.is_empty());
}
