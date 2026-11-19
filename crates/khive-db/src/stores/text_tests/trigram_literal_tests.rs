/// #397 regression: production defaults to the FTS5 `trigram`
/// tokenizer (`backend.rs`'s `StorageBackend::text()`), and generic search
/// (`operations.rs::search_notes`) queries it in `Plain` mode — neither of
/// which the prior `unicode61`/`AnyTerm`-only coverage exercised. Assert
/// exact hit-id sets (not just "contains") for punctuated identifiers,
/// decimals, versions, and legacy-normalized forms, under a real trigram
/// table, in both `Plain` and `AnyTerm` mode.
#[tokio::test]
async fn test_search_trigram_punctuated_and_decimal_queries_matches_exact_ids() {
    let store = setup_trigram_store("trigram_punctuated");

    let hyphen_id = Uuid::new_v4();
    store
        .upsert_document(make_document(hyphen_id, "doc", "khive-pack-memory crate"))
        .await
        .unwrap();

    let dotted_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            dotted_id,
            "doc",
            "the khive.pack.memory module",
        ))
        .await
        .unwrap();

    let decimal_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            decimal_id,
            "doc",
            "salience 0.9 vs 0.3 comparison",
        ))
        .await
        .unwrap();

    let version_id = Uuid::new_v4();
    store
        .upsert_document(make_document(version_id, "doc", "released version 1.2.3"))
        .await
        .unwrap();

    // A literal punctuated identifier short enough (`id` = 2 chars) that its
    // split segments are trigram-unsafe (below FTS5_TRIGRAM_MIN_SAFE_LEN):
    // only reachable via the exact-substring phrase alternative under
    // `trigram`, since the doc body embeds the raw "$prev.id" token verbatim
    // rather than as separately tokenizable words.
    let legacy_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            legacy_id,
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
        for (query, expected_id, label) in [
            ("khive-pack-memory", hyphen_id, "hyphenated identifier"),
            ("khive.pack.memory", dotted_id, "dotted identifier"),
            ("0.9", decimal_id, "decimal"),
            ("1.2.3", version_id, "version string"),
            ("$prev.id", legacy_id, "legacy dollar+dot query"),
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
            assert!(
                hit_ids.contains(&expected_id),
                "trigram {mode:?} query {query:?} ({label}) must match {expected_id}; got {hit_ids:?}"
            );
            assert!(
                !hit_ids.contains(&unrelated_id),
                "trigram {mode:?} query {query:?} ({label}) must not match unrelated doc {unrelated_id}; got {hit_ids:?}"
            );
        }
    }
}

/// #397 concrete broadening regression: a hyphenated date query
/// under the trigram tokenizer must not collapse to matching every document
/// that merely shares the year. `sanitize_fts5_token_group`'s split reading
/// keeps the year term ("2026", 4 chars) fully discriminating under trigram;
/// this test pins that neither the split nor the merged OR-alternative
/// widens the match to a different day in the same year.
#[tokio::test]
async fn test_search_trigram_date_query_does_not_broaden_to_same_year() {
    let store = setup_trigram_store("trigram_date");

    let target_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            target_id,
            "doc",
            "changelog entry dated 2026-07-10",
        ))
        .await
        .unwrap();

    let other_day_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            other_day_id,
            "doc",
            "changelog entry dated 2026-03-15",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        let hits = store
            .search(TextSearchRequest {
                query: "2026-07-10".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .unwrap();
        let hit_ids: std::collections::HashSet<_> = hits.iter().map(|hit| hit.subject_id).collect();
        assert!(
            hit_ids.contains(&target_id),
            "trigram {mode:?} date query must match its own date {target_id}; got {hit_ids:?}"
        );
        assert!(
            !hit_ids.contains(&other_day_id),
            "trigram {mode:?} date query for 2026-07-10 must not broaden to 2026-03-15 \
             ({other_day_id}); got {hit_ids:?}"
        );
    }
}

/// #397 regression: a punctuated operand of an FTS5
/// operator expression (`NEAR(alpha-beta,5)`, `NOT(alpha-beta,5)`) makes the
/// legacy-merged OR-alternative in `sanitize_fts5_token_group` collapse to
/// multiple space-separated terms (`"alphabeta 5"`) instead of one bareword,
/// because the operand's comma spaces the trailing short segment off from
/// the merged word. Pushed unguarded into the OR-group, that fragment's
/// trigram-unsafe `5` term silently drops out under FTS5's implicit-AND
/// adjacency (see `join_plain_groups`'s doc comment), broadening the match
/// to any row containing the bare `alphabeta` merge. Pin under the
/// production trigram tokenizer that an unrelated row containing only the
/// merged bareword does not match.
#[tokio::test]
async fn test_search_trigram_operator_short_operand_does_not_broaden() {
    let store = setup_trigram_store("trigram_operator_short_operand");

    let unrelated_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            unrelated_id,
            "doc",
            "an alphabeta widget completely unrelated to any proximity or negation query",
        ))
        .await
        .unwrap();

    for (query, label) in [
        (
            "NEAR(alpha-beta,5)",
            "NEAR operator with a short numeric operand",
        ),
        (
            "NOT(alpha-beta,5)",
            "NOT operator with a short numeric operand",
        ),
    ] {
        for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
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
            assert!(
                !hit_ids.contains(&unrelated_id),
                "trigram {mode:?} query {query:?} ({label}) must not broaden to match \
                 {unrelated_id} via the trigram-unsafe multi-term merged alternative; \
                 got {hit_ids:?}"
            );
        }
    }
}
