/// The legacy merged alternative applies only to the pre-#397 hyphen/dot
/// behavior. A slash query must not match the unrelated slashless spelling.
#[tokio::test]
async fn test_search_slash_query_excludes_merged_alias_all_modes_and_tokenizers() {
    assert_slash_query_excludes_merged_alias(
        setup_memory_store("unicode61_slash_alias"),
        "unicode61",
    )
    .await;
    assert_slash_query_excludes_merged_alias(setup_trigram_store("trigram_slash_alias"), "trigram")
        .await;
}

#[test]
fn test_is_fts5_bareword_safe() {
    assert!(is_fts5_bareword_safe("hello"));
    assert!(is_fts5_bareword_safe("hello123"));
    assert!(is_fts5_bareword_safe("_prefixed"));
    assert!(is_fts5_bareword_safe("682"));
    assert!(!is_fts5_bareword_safe(""));
    for bad in [
        "#682", "B=128", "K%Prob", "a&b", "a;b", "a<b", "a>b", "a?b", "a@b", "a[b", "a\\b", "a]b",
        "a`b", "a{b", "a|b", "a}b", "a:b", "a-b",
    ] {
        assert!(
            !is_fts5_bareword_safe(bad),
            "{bad:?} must not be treated as a bareword-safe token"
        );
    }
}

#[tokio::test]
async fn test_search_with_hash_sign_does_not_crash_and_matches() {
    let store = setup_trigram_store("hash_sign_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "issue tracker",
            "tracking #682 Stage 2: MoE expert-cache prefetch work",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        let result = store
            .search(TextSearchRequest {
                query: "#682".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "#916 {mode:?} hash-sign query must not crash FTS5, got: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().len(),
            1,
            "#916 {mode:?} hash-sign query must still match the seeded document"
        );
    }
}

#[tokio::test]
async fn test_search_with_equals_sign_does_not_crash_and_matches() {
    let store = setup_trigram_store("equals_sign_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "benchmark notes",
            "chunkwise B=128 traffic arithmetic simdgroup_matrix DPLR",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        let result = store
            .search(TextSearchRequest {
                query: "B=128".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "#916 {mode:?} equals-sign query must not crash FTS5, got: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().len(),
            1,
            "#916 {mode:?} equals-sign query must still match the seeded document"
        );
    }
}

#[tokio::test]
async fn test_search_with_percent_sign_does_not_crash_and_matches() {
    let store = setup_trigram_store("percent_sign_query");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "sampling notes",
            "evaluated with the Min-K%Prob membership inference method",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        let result = store
            .search(TextSearchRequest {
                query: "Min-K%Prob".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "#916 {mode:?} percent-sign query must not crash FTS5, got: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().len(),
            1,
            "#916 {mode:?} percent-sign query must still match the seeded document"
        );
    }
}

#[tokio::test]
async fn test_search_with_embedded_quote_preserves_literal_match() {
    let store = setup_trigram_store("embedded_quote_query");
    let target_id = Uuid::new_v4();
    let unquoted_id = Uuid::new_v4();

    store
        .upsert_document(make_document(
            target_id,
            "quoted identifier",
            "the foo\"bar identifier appears here",
        ))
        .await
        .unwrap();
    store
        .upsert_document(make_document(
            unquoted_id,
            "unquoted identifier",
            "the foobar identifier appears here",
        ))
        .await
        .unwrap();

    for mode in [
        TextQueryMode::Plain,
        TextQueryMode::AnyTerm,
        TextQueryMode::Phrase,
    ] {
        let hits = store
            .search(TextSearchRequest {
                query: "foo\"bar".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        let hit_ids: std::collections::HashSet<_> =
            hits.into_iter().map(|hit| hit.subject_id).collect();

        assert!(
            hit_ids.contains(&target_id),
            "{mode:?} must preserve an embedded quote as literal query text; got {hit_ids:?}"
        );
    }
}

#[tokio::test]
async fn test_search_with_mixed_punctuated_and_plain_tokens_does_not_crash() {
    let store = setup_trigram_store("mixed_punctuated_plain_query");

    let target_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            target_id,
            "issue tracker",
            "tracking #682 Stage 2: MoE expert-cache prefetch work",
        ))
        .await
        .unwrap();

    let unrelated_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            unrelated_id,
            "doc",
            "completely unrelated content about gardening Stage props",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        let result = store
            .search(TextSearchRequest {
                query: "#682 Stage 2".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await;
        assert!(
            result.is_ok(),
            "#916 {mode:?} mixed punctuated+plain query must not crash FTS5, got: {:?}",
            result.err()
        );
        let hit_ids: std::collections::HashSet<_> =
            result.unwrap().into_iter().map(|h| h.subject_id).collect();
        assert!(
            hit_ids.contains(&target_id),
            "#916 {mode:?} mixed query must match the seeded document; got {hit_ids:?}"
        );
    }
}
