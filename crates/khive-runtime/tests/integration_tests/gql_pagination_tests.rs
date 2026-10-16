// =============================================================================
// GQL query truncation warning (issue #777)
//
// The compiler cannot infer truncation from the requested LIMIT alone — it
// must observe whether a real (max_limit + 1)-th match exists. These tests
// exercise the full compile -> execute -> strip-sentinel -> warn pipeline
// against real result sets straddling the default 500-row cap.
// =============================================================================

/// Seed `n` concept entities into a fresh namespace and return the token.
async fn seed_concepts(rt: &KhiveRuntime, ns: &str, n: usize) -> khive_runtime::NamespaceToken {
    let tok = rt.authorize(Namespace::parse(ns).unwrap()).unwrap();
    for i in 0..n {
        rt.create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            &format!("seed-{i}"),
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    }
    tok
}

async fn gql_page(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    query: &str,
    page_size: usize,
) -> khive_runtime::QueryResult {
    rt.query_with_metadata(
        token,
        query,
        khive_query::CompileOptions {
            max_limit: page_size,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn no_explicit_limit_under_and_at_cap_emits_no_warning() {
    let rt = rt();

    // 499 matches, no explicit LIMIT: below the cap, all rows returned, no warning.
    let tok_499 = seed_concepts(&rt, "trunc-499", 499).await;
    let result_499 = rt
        .query_with_metadata(
            &tok_499,
            "MATCH (a:concept) RETURN a",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result_499.rows.len(), 499);
    assert!(
        result_499.warnings.is_empty(),
        "499 matches under the cap must not warn: {:?}",
        result_499.warnings
    );
    assert!(
        !result_499.truncated,
        "#1247: under the cap is not truncated"
    );

    // Exactly 500 matches, no explicit LIMIT: right at the cap, no truncation, no warning.
    let tok_500 = seed_concepts(&rt, "trunc-500", 500).await;
    let result_500 = rt
        .query_with_metadata(
            &tok_500,
            "MATCH (a:concept) RETURN a",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result_500.rows.len(), 500);
    assert!(
        result_500.warnings.is_empty(),
        "exactly 500 matches must not warn (nothing was dropped): {:?}",
        result_500.warnings
    );
    assert!(
        !result_500.truncated,
        "#1247: exactly at the cap is not truncated (nothing was dropped)"
    );
}

#[tokio::test]
async fn no_explicit_limit_over_cap_warns_and_strips_sentinel() {
    let rt = rt();

    // 501 matches, no explicit LIMIT — this is issue #777's original silent-
    // truncation case: the cap is the only bound, and the compiler cannot know
    // ahead of time that a 501st row exists.
    let tok = seed_concepts(&rt, "trunc-501", 501).await;
    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.rows.len(),
        500,
        "sentinel row must be stripped; exactly max_limit rows must be returned"
    );
    assert_eq!(result.warnings.len(), 1, "warnings: {:?}", result.warnings);
    assert!(result.warnings[0].contains("500"), "{}", result.warnings[0]);
    assert!(
        result.warnings[0].contains("SKIP 500") && result.warnings[0].contains("next_offset"),
        "#1601: the warning must agree with the machine-readable GQL continuation: {}",
        result.warnings[0]
    );
    assert!(result.has_more);
    assert_eq!(result.next_offset, Some(500));
    assert!(
        result.truncated,
        "#1247: truncated must be the structural signal, independent of warnings text"
    );

    // The sentinel row must not leak into the returned set: every row must be
    // a distinct seeded entity.
    let names: std::collections::HashSet<_> = result
        .rows
        .iter()
        .filter_map(|r| match r.get("a_name") {
            Some(khive_storage::types::SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 500, "sentinel row must not leak into results");

    let final_page = gql_page(&rt, &tok, "MATCH (a:concept) RETURN a SKIP 500", 500).await;
    assert_eq!((final_page.offset, final_page.page_size), (500, 500));
    assert_eq!(
        final_page.rows.len(),
        1,
        "SKIP 500 must retrieve the 501st match"
    );
    assert!(!final_page.has_more && !final_page.truncated);
    assert_eq!(final_page.next_offset, None);
    let final_name = match final_page.rows[0].get("a_name") {
        Some(khive_storage::types::SqlValue::Text(name)) => name,
        other => panic!("final page must project the remaining entity name, got {other:?}"),
    };
    assert!(
        !names.contains(final_name),
        "the final page must not overlap the first 500 rows"
    );
}

#[tokio::test]
async fn explicit_limit_variants_against_501_matches() {
    let rt = rt();
    let tok = seed_concepts(&rt, "trunc-501-limits", 501).await;

    // LIMIT above the cap: the cap still binds, warning fires, sentinel stripped.
    let above_cap = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a LIMIT 600",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(above_cap.rows.len(), 500);
    assert_eq!(
        above_cap.warnings.len(),
        1,
        "LIMIT above cap with 501 real matches must warn: {:?}",
        above_cap.warnings
    );
    assert!(above_cap.warnings[0].contains("600"));
    assert!(above_cap.warnings[0].contains("500"));
    assert!(
        above_cap.truncated,
        "#1247: LIMIT above cap must set truncated"
    );

    // LIMIT exactly at the cap: the cap never binds (requested <= max_limit),
    // so no sentinel is fetched and no warning fires, even though 501 rows
    // actually match — the caller asked for exactly 500 and got exactly 500.
    let at_cap = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a LIMIT 500",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(at_cap.rows.len(), 500);
    assert!(
        at_cap.warnings.is_empty(),
        "LIMIT == cap must not warn: {:?}",
        at_cap.warnings
    );
    assert!(
        !at_cap.truncated,
        "#1247: an explicit LIMIT the caller chose is not server-side truncation"
    );

    // LIMIT below the cap: this is the false-positive regression
    // case (LIMIT above the requested value but under real matches would have
    // wrongly warned under the old requested-limit-only inference). Here the
    // requested LIMIT is under the cap, so it must never warn regardless of
    // how many rows actually match.
    let below_cap = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a LIMIT 100",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(below_cap.rows.len(), 100);
    assert!(
        below_cap.warnings.is_empty(),
        "LIMIT below cap must not warn: {:?}",
        below_cap.warnings
    );
    assert!(
        !below_cap.truncated,
        "#1247: LIMIT below cap is not truncated"
    );
}

#[tokio::test]
async fn explicit_limit_over_cap_with_few_real_matches_emits_no_warning() {
    let rt = rt();

    // Only 20 real matches, but the explicit LIMIT (600) exceeds the cap
    // (500). This is the false-positive regression case: the old
    // warn-whenever-LIMIT-exceeds-cap inference would have fired here even
    // though nothing was actually truncated. All 20 rows must come back and
    // no warning must fire.
    let tok = seed_concepts(&rt, "trunc-20-limit-over-cap", 20).await;
    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a LIMIT 600",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 20);
    assert!(
        result.warnings.is_empty(),
        "LIMIT above cap with fewer real matches than the cap must not warn: {:?}",
        result.warnings
    );
    assert!(
        !result.truncated,
        "#1247: fewer real matches than the cap is not truncation"
    );
}

#[tokio::test]
async fn gql_skip_pages_are_deterministic_complete_and_machine_continuable() {
    let rt = rt();
    let tok = seed_concepts(&rt, "gql-pages-12", 12).await;

    let names = |result: &khive_runtime::QueryResult| {
        result
            .rows
            .iter()
            .filter_map(|row| match row.get("a_name") {
                Some(khive_storage::types::SqlValue::Text(name)) => Some(name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    let first = gql_page(&rt, &tok, "MATCH (a:concept) RETURN a", 5).await;
    let first_repeat = gql_page(&rt, &tok, "MATCH (a:concept) RETURN a", 5).await;
    let second = gql_page(&rt, &tok, "MATCH (a:concept) RETURN a SKIP 5", 5).await;
    let third = gql_page(&rt, &tok, "MATCH (a:concept) RETURN a SKIP 10", 5).await;

    assert_eq!(
        names(&first),
        names(&first_repeat),
        "page order must be stable"
    );
    assert_eq!((first.offset, first.page_size), (0, 5));
    assert!(first.has_more && first.truncated);
    assert_eq!(first.next_offset, Some(5));
    assert!(
        first
            .warnings
            .iter()
            .any(|warning| warning.contains("SKIP 5")),
        "human guidance must agree with next_offset: {:?}",
        first.warnings
    );

    assert_eq!((second.offset, second.page_size), (5, 5));
    assert!(second.has_more && second.truncated);
    assert_eq!(second.next_offset, Some(10));

    assert_eq!((third.offset, third.page_size), (10, 5));
    assert!(!third.has_more && !third.truncated);
    assert_eq!(third.next_offset, None);
    assert_eq!(third.rows.len(), 2);

    let all_names = names(&first)
        .into_iter()
        .chain(names(&second))
        .chain(names(&third))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        all_names.len(),
        12,
        "paging must retrieve the full match set"
    );
}

#[tokio::test]
async fn gql_query_limit_composes_with_page_size() {
    let rt = rt();
    let tok = seed_concepts(&rt, "gql-page-limit-composition", 8).await;

    let caller_bounded = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a LIMIT 3",
            khive_query::CompileOptions {
                max_limit: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(caller_bounded.rows.len(), 3);
    assert_eq!(caller_bounded.page_size, 3);
    assert!(!caller_bounded.has_more);
    assert_eq!(caller_bounded.next_offset, None);

    let server_bounded = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a LIMIT 8",
            khive_query::CompileOptions {
                max_limit: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(server_bounded.rows.len(), 5);
    assert_eq!(server_bounded.page_size, 5);
    assert!(server_bounded.has_more);
    assert_eq!(server_bounded.next_offset, Some(5));

    // Continue to the terminal page: only 8 real matches exist and LIMIT is 8,
    // so SKIP 5 must return the remaining 3 rows and terminate.
    let terminal = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept) RETURN a SKIP 5 LIMIT 8",
            khive_query::CompileOptions {
                max_limit: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(terminal.rows.len(), 3);
    assert!(!terminal.has_more);
    assert_eq!(terminal.next_offset, None);
}

/// Regression test for the round-1 review defect: an explicit query-text LIMIT
/// must compose with page_size as a TOTAL bound across every SKIP page, not a
/// page-local one (ADR-008 §"query LIMIT and page_size composition"). Seeds
/// more real matches than LIMIT so a page-local limit would leak rows past
/// LIMIT and keep reporting has_more=true, which is exactly the bug this test
/// pins down.
#[tokio::test]
async fn gql_query_limit_is_a_total_bound_across_skip_pages() {
    let rt = rt();
    let tok = seed_concepts(&rt, "gql-page-limit-total-bound", 20).await;

    let mut collected_ids = std::collections::HashSet::new();
    let mut page_counts = Vec::new();
    let mut offset = 0usize;
    loop {
        let page = gql_page(
            &rt,
            &tok,
            &format!("MATCH (a:concept) RETURN a SKIP {offset} LIMIT 8"),
            5,
        )
        .await;
        page_counts.push(page.rows.len());
        for row in &page.rows {
            if let Some(khive_storage::types::SqlValue::Text(id)) = row.get("a_id") {
                assert!(
                    collected_ids.insert(id.clone()),
                    "row {id} returned on more than one page"
                );
            }
        }
        match page.next_offset {
            Some(next) => {
                assert!(page.has_more);
                offset = next;
            }
            None => {
                assert!(!page.has_more, "terminal page must not claim has_more");
                break;
            }
        }
        assert!(
            page_counts.len() <= 10,
            "paging did not terminate: {page_counts:?}"
        );
    }

    assert_eq!(
        collected_ids.len(),
        8,
        "exactly LIMIT rows must be returned across all pages, not the 20 real matches"
    );
    assert_eq!(
        page_counts,
        vec![5, 3],
        "page sizes must respect the remaining LIMIT allowance, not a flat page_size"
    );

    // An offset at or beyond the query's own LIMIT is a terminal empty page,
    // never an error and never has_more.
    let past_limit = gql_page(&rt, &tok, "MATCH (a:concept) RETURN a SKIP 8 LIMIT 8", 5).await;
    assert_eq!(past_limit.rows.len(), 0);
    assert!(!past_limit.has_more);
    assert_eq!(past_limit.next_offset, None);

    let well_past_limit =
        gql_page(&rt, &tok, "MATCH (a:concept) RETURN a SKIP 20 LIMIT 8", 5).await;
    assert_eq!(well_past_limit.rows.len(), 0);
    assert!(!well_past_limit.has_more);
    assert_eq!(well_past_limit.next_offset, None);
}
