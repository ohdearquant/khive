#[tokio::test]
async fn test_sanitize_fts5_query() {
    assert_eq!(sanitize_fts5_query("hello world"), "hello world");
    assert_eq!(sanitize_fts5_query("hello*world"), "helloworld");
    assert_eq!(sanitize_fts5_query("\"quoted\""), "quoted");
    assert_eq!(sanitize_fts5_query("(parens)"), "parens");
    assert_eq!(sanitize_fts5_query("a + b - c"), "a b c");
    assert_eq!(sanitize_fts5_query("col:value"), "col value");
    assert_eq!(sanitize_fts5_query(""), "");
    assert_eq!(sanitize_fts5_query("***"), "");
    // M-C4: decimal numbers must not produce "syntax error near '.'"
    assert_eq!(
        sanitize_fts5_query("salience 0.9 vs 0.3"),
        "salience 0 9 vs 0 3"
    );
    assert_eq!(sanitize_fts5_query("version 1.2.3"), "version 1 2 3");
    // #397: hyphenated and dotted identifiers must space-split, not concatenate.
    assert_eq!(
        sanitize_fts5_query("khive-pack-memory"),
        "khive pack memory"
    );
    assert_eq!(
        sanitize_fts5_query("khive.pack.memory"),
        "khive pack memory"
    );
    // H1: tilde and comma must be stripped to prevent FTS5 syntax errors
    assert_eq!(sanitize_fts5_query("~hello"), "hello");
    assert_eq!(sanitize_fts5_query("\"+_~!\""), "_");
    assert_eq!(sanitize_fts5_query("NEAR(smile, 5)"), "smile 5");
    assert_eq!(sanitize_fts5_query("a,b,c"), "a b c");
    // Full operator-class matrix
    // Apostrophe fix: single quote is an FTS5 string-literal delimiter in Plain mode.
    assert_eq!(sanitize_fts5_query("Bob's tenant"), "Bobs tenant");
    assert_eq!(
        sanitize_fts5_query("tenant AND isolation"),
        "tenant isolation"
    );
    assert_eq!(
        sanitize_fts5_query("tenant OR isolation"),
        "tenant isolation"
    );
    assert_eq!(
        sanitize_fts5_query("tenant NOT isolation"),
        "tenant isolation"
    );
    assert_eq!(
        sanitize_fts5_query("tenant NEAR(isolation, 5)"),
        "tenant isolation 5"
    );
    assert_eq!(sanitize_fts5_query("tenant:isolation"), "tenant isolation");
    assert_eq!(
        sanitize_fts5_query("tenant ^ isolation"),
        "tenant isolation"
    );
    assert_eq!(
        sanitize_fts5_query("(tenant isolation)"),
        "tenant isolation"
    );
    // whitespace-only becomes empty
    assert_eq!(sanitize_fts5_query("   "), "");
    // operator-only after stripping becomes empty
    assert_eq!(sanitize_fts5_query("AND OR NOT"), "");
    // #388: dollar sign is an unconditional FTS5 MATCH-parser syntax error
    // ("syntax error near \"$\"") regardless of position in the token or query.
    assert_eq!(sanitize_fts5_query("$prev.id"), "prev id");
    assert_eq!(sanitize_fts5_query("$prev"), "prev");
    assert_eq!(sanitize_fts5_query("foo$bar"), "foobar");
    assert_eq!(sanitize_fts5_query("$"), "");
    assert_eq!(sanitize_fts5_query("$$"), "");
}

/// H1 regression: queries with tilde (~) must not produce "fts5: syntax error near '~'".
#[tokio::test]
async fn test_search_with_tilde_does_not_crash() {
    let store = setup_memory_store("tilde_query");

    store
        .upsert_document(make_document(Uuid::new_v4(), "smile", "smiling face"))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "~smile".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "tilde query must not crash FTS5, got: {:?}",
        result.err()
    );
}

/// H1 regression: NEAR() queries must not produce "fts5: syntax error near ','".
#[tokio::test]
async fn test_search_with_near_operator_does_not_crash() {
    let store = setup_memory_store("near_query");

    store
        .upsert_document(make_document(Uuid::new_v4(), "smile", "quokka smile happy"))
        .await
        .unwrap();

    let result = store
        .search(TextSearchRequest {
            query: "quokka NEAR(smile, 5)".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "NEAR() query must not crash FTS5, got: {:?}",
        result.err()
    );
}

/// M-C4 regression: searching with decimal numbers must succeed (not crash FTS5).
///
/// Previously `.` was not stripped, causing FTS5 to return
/// "fts5: syntax error near '.'" when queries contained decimal literals like "0.9".
#[tokio::test]
async fn test_search_with_decimal_query_does_not_crash() {
    let store = setup_memory_store("decimal_query");

    // Insert a document that contains decimal-like content.
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "salience thresholds",
            "salience 0 9 vs 0 3 comparison",
        ))
        .await
        .unwrap();

    // Must not return an error — previously "fts5: syntax error near '.'"
    let result = store
        .search(TextSearchRequest {
            query: "salience 0.9 vs 0.3".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result.is_ok(),
        "decimal query must succeed, got error: {:?}",
        result.err()
    );

    // Also test with version strings.
    let result2 = store
        .search(TextSearchRequest {
            query: "salience 0.9 vs version 1.2.3".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await;
    assert!(
        result2.is_ok(),
        "version-string query must succeed, got error: {:?}",
        result2.err()
    );
}

/// #397 regression: punctuated identifier queries must not be concatenated into
/// tokens that cannot occur in the indexed text.
#[tokio::test]
async fn test_search_with_hyphenated_and_dotted_queries_matches_literal_tokens() {
    let store = setup_memory_store("punctuated_query_match");

    let hyphen_id = Uuid::new_v4();
    store
        .upsert_document(make_document(hyphen_id, "doc", "LEGACY-FLAT-NOTE"))
        .await
        .unwrap();

    let dotted_id = Uuid::new_v4();
    store
        .upsert_document(make_document(dotted_id, "doc", "khive.pack.memory"))
        .await
        .unwrap();

    for (query, expected_id) in [
        ("LEGACY-FLAT-NOTE", hyphen_id),
        ("khive.pack.memory", dotted_id),
    ] {
        let hits = store
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::AnyTerm,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        let hit_ids: Vec<_> = hits.iter().map(|hit| hit.subject_id).collect();
        assert!(
            hit_ids.contains(&expected_id),
            "#397 query {query:?} must match {expected_id}; got {hit_ids:?}"
        );
    }
}

/// Regression: `sanitize_fts5_token_group` must keep
/// the legacy-merged bareword alternative reachable for ordinary punctuated
/// identifiers under `unicode61`. The merged form (`khivepackmemory`,
/// `previd`) is content indexed before #397's split-terms change, or content
/// whose own tokenizer collapsed punctuation the same way; a query for the
/// punctuated spelling must still find it. Assert exact hit-id sets (not
/// just "contains") so a fix that accidentally broadens the match — e.g. by
/// dropping the trigram-safety gate on multi-term merges — is caught too.
#[tokio::test]
async fn test_search_matches_legacy_merged_and_punctuated_forms_exact_ids() {
    let store = setup_memory_store("legacy_merged_punctuated");

    let legacy_merged_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            legacy_merged_id,
            "doc",
            "khivepackmemory legacy note",
        ))
        .await
        .unwrap();

    let punctuated_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            punctuated_id,
            "doc",
            "khive-pack-memory crate",
        ))
        .await
        .unwrap();

    let legacy_prev_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            legacy_prev_id,
            "DSL docs",
            "chain results with previd token",
        ))
        .await
        .unwrap();

    let punctuated_prev_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            punctuated_prev_id,
            "DSL docs",
            "chain results with the $prev.id token",
        ))
        .await
        .unwrap();

    let unrelated_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            unrelated_id,
            "doc",
            "completely unrelated content about gardening",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        for (query, expected_ids, label) in [
            (
                "khive-pack-memory",
                vec![legacy_merged_id, punctuated_id],
                "punctuated identifier reaches both legacy-merged and split forms",
            ),
            (
                "$prev.id",
                vec![legacy_prev_id, punctuated_prev_id],
                "dollar+dot query reaches both legacy-merged and split forms",
            ),
        ] {
            let hits = store
                .search(TextSearchRequest {
                    query: query.to_string(),
                    mode: mode.clone(),
                    filter: Some(ns_filter("test_ns")),
                    top_k: 10,
                    snippet_chars: 0,
                })
                .await
                .unwrap();
            let hit_ids: std::collections::HashSet<_> =
                hits.iter().map(|hit| hit.subject_id).collect();
            let expected: std::collections::HashSet<_> = expected_ids.into_iter().collect();
            assert_eq!(
                hit_ids, expected,
                "unicode61 {mode:?} query {query:?} ({label}) must match exactly {expected:?}; got {hit_ids:?}"
            );
            assert!(
                !hit_ids.contains(&unrelated_id),
                "unicode61 {mode:?} query {query:?} ({label}) must not match unrelated doc {unrelated_id}; got {hit_ids:?}"
            );
        }
    }
}
