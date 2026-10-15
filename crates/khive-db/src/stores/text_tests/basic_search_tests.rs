#[tokio::test]
async fn test_upsert_and_search() {
    let store = setup_memory_store("upsert_search");

    let id = Uuid::new_v4();
    let doc = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Entity,
        record_kind: None,
        title: Some("Rust Programming".to_string()),
        body: "Rust is a systems programming language focused on safety and performance."
            .to_string(),
        tags: vec!["rust".to_string(), "programming".to_string()],
        namespace: "tech".to_string(),
        metadata: None,
        updated_at: Utc::now(),
    };

    store.upsert_document(doc).await.unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "Rust programming".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("tech")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id);
    assert_eq!(hits[0].rank, 1);
    assert!(hits[0].score.to_f64() > 0.0);
    assert!(hits[0].title.is_some());
}

#[tokio::test]
async fn test_phrase_search() {
    let store = setup_memory_store("phrase");

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    store
        .upsert_document(make_document(
            id1,
            "Animals",
            "The quick brown fox jumps over the lazy dog.",
        ))
        .await
        .unwrap();

    store
        .upsert_document(make_document(
            id2,
            "Colors",
            "The brown paint was quick to dry, unlike the fox.",
        ))
        .await
        .unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "quick brown fox".to_string(),
            mode: TextQueryMode::Phrase,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id1);

    let hits = store
        .search(TextSearchRequest {
            query: "quick brown fox".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 2);
}
