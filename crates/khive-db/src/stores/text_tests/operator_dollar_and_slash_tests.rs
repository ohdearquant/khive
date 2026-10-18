/// All FTS5 operator classes must not crash the generic text search surface.
#[tokio::test]
async fn test_search_with_fts_operator_matrix_does_not_crash() {
    let store = setup_memory_store("fts_operator_matrix");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "tenant isolation",
            "multi-tenant isolation operator regression anchor content",
        ))
        .await
        .unwrap();

    let cases: &[&str] = &[
        "\"tenant isolation\"",
        "Bob \"quoted\" tenant",
        "tenant AND isolation",
        "tenant OR isolation",
        "tenant NOT isolation",
        "tenant NEAR(isolation, 5)",
        "tenant*",
        "***",
        "tenant:isolation",
        "tenant ^ isolation",
        "(tenant isolation)",
        "(\"+_~!\")",
        "tenant:foo^bar*",
        "multi-tenant isolation",
        "   ",
        "",
    ];

    for query in cases {
        let result = store
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "#570 DB search query {query:?} must not crash FTS5, got: {:?}",
            result.err()
        );
    }
}

/// #388 regression: a bareword `$` query (e.g. the DSL doc query `$prev.id`) must not
/// crash the FTS5 leg. Previously `$` was untouched by `sanitize_fts5_query`, so it
/// reached FTS5 raw and produced `fts5: syntax error near "$"`, aborting the whole
/// search instead of degrading.
#[tokio::test]
async fn test_search_with_dollar_sign_does_not_crash() {
    let store = setup_memory_store("dollar_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "DSL docs",
            "chain results with the prev id token",
        ))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "$prev.id".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "#388 dollar-sign query must not crash FTS5, got: {:?}",
        result.err()
    );
    // sanitize_fts5_query("$prev.id") == "prev id" ('$' stripped, '.' space-split),
    // confirming legitimate text search stays intact after sanitization.
    assert_eq!(result.unwrap().len(), 1);
}

/// #388 regression: a bareword query consisting solely of `$` sanitizes to an empty
/// match expression. `search()` must short-circuit to an empty result set rather than
/// sending an empty/invalid MATCH string to FTS5.
#[tokio::test]
async fn test_search_with_bare_dollar_returns_empty_not_error() {
    let store = setup_memory_store("bare_dollar_query");

    store
        .upsert_document(make_document(Uuid::new_v4(), "doc", "some content"))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "$".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "#388 bare-$ query must not crash FTS5, got: {:?}",
        result.err()
    );
    assert!(result.unwrap().is_empty());
}

/// #388 regression: `$` combined with an embedded quote must not crash the FTS5 leg
/// either, exercising both the apostrophe and dollar-sign (#388) fixes together.
#[tokio::test]
async fn test_search_with_dollar_and_quote_does_not_crash() {
    let store = setup_memory_store("dollar_quote_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "mixed",
            "operator syntax reference content",
        ))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "$prev \"operator syntax\"".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "#388 dollar+quote query must not crash FTS5, got: {:?}",
        result.err()
    );
}

/// #388 regression: `AnyTerm` mode (used by memory.recall fanout) must also survive a
/// `$`-bearing query — this mode sanitizes each term independently before joining with OR.
#[tokio::test]
async fn test_search_any_term_mode_with_dollar_does_not_crash() {
    let store = setup_memory_store("dollar_any_term_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "DSL docs",
            "chain results with prev id",
        ))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "$prev id".to_string(),
            mode: TextQueryMode::AnyTerm,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "#388 AnyTerm dollar query must not crash FTS5, got: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().len(), 1);
}

/// Slash-bearing query (e.g. `GB/s throughput measurements`) must not crash the FTS5
/// leg. `/` was previously untouched by `sanitize_fts5_query`, reaching FTS5 raw and
/// producing `fts5: syntax error near "/"`.
#[tokio::test]
async fn test_search_with_slash_does_not_crash_and_matches() {
    let store = setup_memory_store("slash_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "bandwidth benchmark",
            "measured 900 GB/s throughput on the device",
        ))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "GB/s throughput".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "slash query must not crash FTS5, got: {:?}",
        result.err()
    );
    assert_eq!(
        result.unwrap().len(),
        1,
        "slash query must still return the seeded matching document"
    );
}

/// AnyTerm mode (used by memory.recall fanout) must also survive a slash-bearing query.
#[tokio::test]
async fn test_search_any_term_mode_with_slash_does_not_crash() {
    let store = setup_memory_store("slash_any_term_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "bandwidth benchmark",
            "measured 900 GB/s throughput on the device",
        ))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "GB/s".to_string(),
            mode: TextQueryMode::AnyTerm,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "AnyTerm slash query must not crash FTS5, got: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().len(), 1);
}

/// Phrase mode must preserve slash punctuation so the production trigram
/// tokenizer matches the literal substring rather than a space-split phrase.
#[tokio::test]
async fn test_search_trigram_phrase_with_slash_returns_exact_literal_id() {
    let store = setup_trigram_store("trigram_phrase_slash_literal");
    let target_id = Uuid::new_v4();
    let spaced_distractor_id = Uuid::new_v4();

    store
        .upsert_document(make_document(
            target_id,
            "slash literal",
            "measured 900 GB/s throughput on the device",
        ))
        .await
        .unwrap();
    store
        .upsert_document(make_document(
            spaced_distractor_id,
            "space-separated distractor",
            "measured 900 GB s throughput on the device",
        ))
        .await
        .unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "GB/s throughput".to_string(),
            mode: TextQueryMode::Phrase,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();
    let hit_ids: std::collections::HashSet<_> = hits.iter().map(|hit| hit.subject_id).collect();

    assert_eq!(
        hit_ids,
        std::collections::HashSet::from([target_id]),
        "trigram Phrase query must return only its slash-bearing literal; got {hit_ids:?}"
    );
}
