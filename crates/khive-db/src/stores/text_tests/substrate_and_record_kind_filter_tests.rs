#[tokio::test]
async fn test_search_with_kind_filter() {
    let store = setup_memory_store("filter_kind");

    let id_entity = Uuid::new_v4();
    let id_note = Uuid::new_v4();

    store
        .upsert_document(TextDocument {
            subject_id: id_entity,
            kind: SubstrateKind::Entity,
            record_kind: None,
            title: Some("Rust Guide".to_string()),
            body: "A comprehensive guide to Rust programming.".to_string(),
            tags: vec![],
            namespace: "test_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    store
        .upsert_document(TextDocument {
            subject_id: id_note,
            kind: SubstrateKind::Note,
            record_kind: None,
            title: Some("Rust Notes".to_string()),
            body: "Quick notes about Rust concepts.".to_string(),
            tags: vec![],
            namespace: "test_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "Rust".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(TextFilter {
                kinds: vec![SubstrateKind::Entity],
                namespaces: vec!["test_ns".to_string()],
                ..TextFilter::default()
            }),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id_entity);
}

#[tokio::test]
async fn record_kind_filter_preserves_results_and_scopes_every_gather_mode() {
    let store = setup_trigram_store("record_kind_gather_modes");
    let memory_ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for id in memory_ids {
        store
            .upsert_document(make_record_kind_document(
                id,
                "memory",
                "recall fixture",
                "zircon recall evidence",
            ))
            .await
            .unwrap();
    }

    let request = |record_kinds: Vec<String>| TextSearchRequest {
        query: "zircon".to_string(),
        mode: TextQueryMode::Plain,
        filter: Some(TextFilter {
            namespaces: vec!["test_ns".to_string()],
            record_kinds,
            ..TextFilter::default()
        }),
        top_k: 10,
        snippet_chars: 0,
    };
    let before_noise = store.search(request(Vec::new())).await.unwrap();
    assert_eq!(before_noise.len(), memory_ids.len());
    let mut classifier_only_query = request(Vec::new());
    classifier_only_query.query = "memory".to_string();
    assert!(
        store
            .search(classifier_only_query)
            .await
            .unwrap()
            .is_empty(),
        "an unscoped lexical query must not match indexed classifier text"
    );

    for i in 0..100 {
        store
            .upsert_document(make_record_kind_document(
                Uuid::new_v4(),
                "message",
                &format!("unrelated {i}"),
                "zircon recall evidence",
            ))
            .await
            .unwrap();
    }

    let ranked = store
        .search(request(vec!["memory".to_string()]))
        .await
        .unwrap();
    assert_eq!(
        ranked.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
        before_noise
            .iter()
            .map(|hit| hit.subject_id)
            .collect::<Vec<_>>(),
        "indexed corpus scoping must preserve the memory-only result set and order"
    );

    for options in [
        TextSearchOptions {
            gather_mode: khive_storage::types::TextGatherMode::Unranked,
            gather_limit: None,
        },
        TextSearchOptions {
            gather_mode: khive_storage::types::TextGatherMode::RankWithinCap,
            gather_limit: Some(20),
        },
    ] {
        let hits = store
            .search_with_options(request(vec!["memory".to_string()]), options)
            .await
            .unwrap();
        assert_eq!(hits.len(), memory_ids.len());
        assert!(
            hits.iter().all(|hit| memory_ids.contains(&hit.subject_id)),
            "every gather mode must honor the indexed record-kind corpus"
        );
    }
}

#[tokio::test]
async fn record_kind_filter_scopes_count_term_stats_and_short_kind_fallback() {
    let store = setup_trigram_store("record_kind_counts");
    for (record_kind, body) in [
        ("memory", "common rare recall"),
        ("memory", "common recall"),
        ("message", "common rare message"),
        ("message", "common rare message"),
        ("ai", "common rare short classifier"),
    ] {
        store
            .upsert_document(make_record_kind_document(
                Uuid::new_v4(),
                record_kind,
                "fixture",
                body,
            ))
            .await
            .unwrap();
    }

    let memory_filter = TextFilter {
        namespaces: vec!["test_ns".to_string()],
        record_kinds: vec!["memory".to_string()],
        ..TextFilter::default()
    };
    assert_eq!(store.count(memory_filter.clone()).await.unwrap(), 2);
    let stats = store
        .term_stats(TextTermStatsRequest {
            terms: vec!["common".to_string(), "rare".to_string()],
            filter: Some(memory_filter),
        })
        .await
        .unwrap();
    let common = stats.iter().find(|stat| stat.term == "common").unwrap();
    let rare = stats.iter().find(|stat| stat.term == "rare").unwrap();
    assert_eq!(common.document_count, 2);
    assert_eq!(common.document_frequency, 2);
    assert_eq!(rare.document_frequency, 1);

    let short_kind_hits = store
        .search(TextSearchRequest {
            query: "common".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(TextFilter {
                namespaces: vec!["test_ns".to_string()],
                record_kinds: vec!["ai".to_string()],
                ..TextFilter::default()
            }),
            top_k: 10,
            snippet_chars: 0,
        })
        .await
        .unwrap();
    assert_eq!(
        short_kind_hits.len(),
        1,
        "a classifier too short for trigram postings must retain exact-filter correctness"
    );
}
