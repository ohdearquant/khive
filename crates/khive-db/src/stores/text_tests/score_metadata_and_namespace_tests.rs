#[tokio::test]
async fn test_score_is_bounded() {
    let store = setup_memory_store("score_bounds");

    for i in 0..5 {
        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                &format!("Doc {}", i),
                &format!("This document discusses topic number {}", i),
            ))
            .await
            .unwrap();
    }

    let hits = store
        .search(TextSearchRequest {
            query: "document topic".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    for hit in &hits {
        let score = hit.score.to_f64();
        assert!(
            score > 0.0 && score <= 1.0,
            "score out of (0, 1] range: {}",
            score
        );
    }

    for (i, hit) in hits.iter().enumerate() {
        assert_eq!(hit.rank, (i + 1) as u32);
    }
}

#[tokio::test]
async fn test_rename_namespace() {
    let store = setup_memory_store("rename_ns");

    let id = Uuid::new_v4();
    let doc = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Note,
        record_kind: None,
        title: Some("Rename test".to_string()),
        body: "keyword_unique_xyz".to_string(),
        tags: vec![],
        namespace: "old_ns".to_string(),
        metadata: None,
        updated_at: Utc::now(),
    };
    store.upsert_document(doc).await.unwrap();

    let before = store
        .search(TextSearchRequest {
            query: "keyword_unique_xyz".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("old_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();
    assert_eq!(before.len(), 1);

    let moved = store.rename_namespace("old_ns", "new_ns").await.unwrap();
    assert_eq!(moved, 1);

    let after_new = store
        .search(TextSearchRequest {
            query: "keyword_unique_xyz".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("new_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();
    assert_eq!(after_new.len(), 1);

    let after_old = store
        .search(TextSearchRequest {
            query: "keyword_unique_xyz".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("old_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();
    assert!(after_old.is_empty());
}

#[tokio::test]
async fn test_metadata_none_roundtrip() {
    let store = setup_memory_store("meta_none");
    let id = uuid::Uuid::new_v4();
    let doc = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Note,
        record_kind: None,
        namespace: "test_ns".to_string(),
        title: None,
        body: "no metadata".to_string(),
        tags: vec![],
        metadata: None,
        updated_at: Utc::now(),
    };
    store.upsert_document(doc).await.unwrap();
    let fetched = store.get_document("test_ns", id).await.unwrap().unwrap();
    assert!(fetched.metadata.is_none());
}

#[tokio::test]
async fn test_rename_namespace_noop() {
    let store = setup_memory_store("rename_noop");

    let id = Uuid::new_v4();
    let doc = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Note,
        record_kind: None,
        title: None,
        body: "noop_test_content".to_string(),
        tags: vec![],
        namespace: "same_ns".to_string(),
        metadata: None,
        updated_at: Utc::now(),
    };
    store.upsert_document(doc).await.unwrap();

    let moved = store.rename_namespace("same_ns", "same_ns").await.unwrap();
    assert_eq!(moved, 0);

    let hits = store
        .search(TextSearchRequest {
            query: "noop_test_content".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("same_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
}
