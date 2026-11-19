use super::*;

// The former two-statement implementation is a reference only for fixtures
// whose subject IDs are globally unique. Its second lookup is not a correct
// oracle for the same UUID in distinct namespaces.
fn legacy_unique_id_hits(
    store: &Fts5TextSearch,
    request: TextSearchRequest,
    gather_limit: u32,
) -> Vec<TextSearchHit> {
    let writer = store.pool.writer().unwrap();
    let conn = writer.conn();
    let table = &store.table_name;
    let expression =
        build_filtered_match_expr(&request.query, request.mode, request.filter.as_ref()).unwrap();
    let (filter, params) = request
        .filter
        .as_ref()
        .map(|filter| build_filter_clause(filter, table, 3))
        .unwrap_or_default();
    let ids = {
        let mut statement = conn
            .prepare(&format!(
                "SELECT subject_id FROM {table} WHERE {table} MATCH ?1{filter} LIMIT ?2"
            ))
            .unwrap();
        statement.raw_bind_parameter(1, &expression).unwrap();
        statement
            .raw_bind_parameter(2, i64::from(gather_limit))
            .unwrap();
        for (i, param) in params.iter().enumerate() {
            statement.raw_bind_parameter(3 + i, &**param).unwrap();
        }
        let mut rows = statement.raw_query();
        let mut ids = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            ids.push(row.get::<_, String>(0).unwrap());
        }
        ids
    };
    if ids.is_empty() {
        return Vec::new();
    }
    let snippet = if request.snippet_chars == 0 {
        "NULL".to_string()
    } else {
        let chars = i32::try_from(request.snippet_chars).unwrap_or(i32::MAX);
        format!("snippet({table}, 3, '', '', '...', {chars})")
    };
    let placeholders = (0..ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statement = conn
        .prepare(&format!(
            "SELECT subject_id, rank, title, {snippet} FROM {table} \
             WHERE {table} MATCH ?1 AND rank MATCH '{LEXICAL_BM25_RANK}' \
             AND subject_id IN ({placeholders}) ORDER BY rank LIMIT ?2"
        ))
        .unwrap();
    statement.raw_bind_parameter(1, &expression).unwrap();
    statement
        .raw_bind_parameter(2, i64::from(request.top_k))
        .unwrap();
    for (i, id) in ids.iter().enumerate() {
        statement.raw_bind_parameter(3 + i, id).unwrap();
    }
    let mut rows = statement.raw_query();
    let mut raw = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        raw.push((
            Uuid::parse_str(&row.get::<_, String>(0).unwrap()).unwrap(),
            row.get::<_, f64>(1).unwrap(),
            row.get::<_, String>(2).unwrap(),
            row.get::<_, Option<String>>(3).unwrap(),
        ));
    }
    let min = raw.iter().map(|hit| hit.1).fold(f64::INFINITY, f64::min);
    let max = raw
        .iter()
        .map(|hit| hit.1)
        .fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;
    raw.into_iter()
        .enumerate()
        .map(|(i, (subject_id, raw_rank, title, snippet))| {
            let score = if range.abs() < 1e-12 {
                1.0
            } else {
                let t = (max - raw_rank) / range;
                0.05 + 0.95 * t
            };
            TextSearchHit {
                subject_id,
                score: DeterministicScore::from_f64(score),
                rank: (i + 1) as u32,
                title: if title.is_empty() { None } else { Some(title) },
                snippet: snippet.filter(|value| !value.is_empty()),
            }
        })
        .collect()
}

fn request(top_k: u32, snippet_chars: usize) -> TextSearchRequest {
    TextSearchRequest {
        query: "needle".to_string(),
        mode: TextQueryMode::Plain,
        filter: Some(ns_filter("test_ns")),
        top_k,
        snippet_chars,
    }
}

fn options(gather_limit: Option<u32>) -> TextSearchOptions {
    TextSearchOptions {
        gather_mode: TextGatherMode::RankWithinCap,
        gather_limit,
    }
}

#[tokio::test]
async fn bounded_ranking_preserves_complete_unique_id_results_and_ties() {
    for trigram in [false, true] {
        let store = if trigram {
            setup_trigram_store("bounded_full_trigram")
        } else {
            setup_memory_store("bounded_full_unicode")
        };
        for i in 0..12u128 {
            // Descending UUIDs distinguish physical-row tie order from UUID order.
            let title = if i < 3 { "tied" } else { "" };
            let body = if i < 3 {
                "needle phrase identical tied body".to_string()
            } else {
                format!(
                    "{}{}",
                    "needle phrase ".repeat(i as usize),
                    "padding ".repeat(20)
                )
            };
            store
                .upsert_document(make_record_kind_document(
                    Uuid::from_u128(100 - i),
                    "memory",
                    title,
                    &body,
                ))
                .await
                .unwrap();
        }
        for mode in [
            TextQueryMode::Plain,
            TextQueryMode::Phrase,
            TextQueryMode::AnyTerm,
        ] {
            for (top_k, cap) in [(1, None), (3, Some(1)), (5, Some(8)), (12, Some(20))] {
                for snippet_chars in [0, 64] {
                    let mut req = request(top_k, snippet_chars);
                    req.mode = mode.clone();
                    req.query = "needle phrase".to_string();
                    req.filter.as_mut().unwrap().record_kinds = vec!["memory".to_string()];
                    let expected =
                        legacy_unique_id_hits(&store, req.clone(), cap.unwrap_or(top_k).max(top_k));
                    let actual = store.search_with_options(req, options(cap)).await.unwrap();
                    assert_eq!(actual.len(), top_k as usize);
                    assert_eq!(
                        serde_json::to_value(&actual).unwrap(),
                        serde_json::to_value(&expected).unwrap(),
                        "full-hit mismatch: trigram={trigram}, mode={mode:?}, top_k={top_k}, cap={cap:?}, snippet={snippet_chars}"
                    );
                    assert_eq!(
                        actual.iter().all(|hit| hit.snippet.is_none()),
                        snippet_chars == 0
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn bounded_gather_precedes_ranking_and_normalizes_only_returned_hits() {
    let store = setup_memory_store("bounded_before_rank");
    let ids = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
    for (id, padding) in ids.into_iter().zip([100, 30, 0]) {
        store
            .upsert_document(make_document(
                id,
                "",
                &format!("needle {}", "padding ".repeat(padding)),
            ))
            .await
            .unwrap();
    }
    let full = store.search(request(3, 64)).await.unwrap();
    assert_eq!(full.len(), 3);
    assert_eq!(
        full[0].subject_id, ids[2],
        "fixture must put the global winner after the cap"
    );
    let bounded = store
        .search_with_options(request(1, 64), options(Some(2)))
        .await
        .unwrap();
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].subject_id, ids[1]);
    assert_eq!(bounded[0].score, DeterministicScore::from_f64(1.0));

    let top_two = store
        .search_with_options(request(2, 64), options(Some(3)))
        .await
        .unwrap();
    assert_eq!(top_two.len(), 2);
    assert_eq!(top_two[1].subject_id, ids[1]);
    assert_eq!(top_two[1].score, DeterministicScore::from_f64(0.05));
    assert!(
        full[1].score > top_two[1].score,
        "normalizing the whole gather must be distinguishable"
    );

    // Both namespaces are eligible, but the first-stage cap selects only one
    // physical row. Looking up that UUID again must not promote its later twin.
    let duplicates = setup_memory_store("bounded_eligible_duplicate");
    let id = Uuid::from_u128(50);
    duplicates
        .upsert_document(make_document(
            id,
            "first physical row",
            &format!("needle {}", "padding ".repeat(100)),
        ))
        .await
        .unwrap();
    let mut later = make_document(id, "later stronger row", "needle");
    later.namespace = "other".to_string();
    duplicates.upsert_document(later).await.unwrap();
    let mut req = request(1, 64);
    req.filter
        .as_mut()
        .unwrap()
        .namespaces
        .push("other".to_string());
    let global = duplicates.search(req.clone()).await.unwrap();
    assert_eq!(global.len(), 1);
    assert_eq!(global[0].title.as_deref(), Some("later stronger row"));
    let capped = duplicates
        .search_with_options(req, options(Some(1)))
        .await
        .unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].subject_id, id);
    assert_eq!(capped[0].title.as_deref(), Some("first physical row"));
}

#[tokio::test]
async fn bounded_ranking_does_not_expand_duplicate_ids_outside_exact_filters() {
    for case in ["namespace", "substrate", "short_kind"] {
        let store = setup_trigram_store(case);
        let id = Uuid::from_u128(10);
        let mut local = make_record_kind_document(id, "ai", "local title", "zircon local evidence");
        local.namespace = "local".to_string();
        let mut foreign =
            make_record_kind_document(id, "memory", "foreign title", "zircon foreign evidence");
        foreign.namespace = "foreign".to_string();
        foreign.kind = SubstrateKind::Entity;
        store.upsert_document(local).await.unwrap();
        store.upsert_document(foreign).await.unwrap();
        assert_eq!(store.count(TextFilter::default()).await.unwrap(), 2);
        let mut filter = TextFilter::default();
        match case {
            "namespace" => filter.namespaces = vec!["local".to_string()],
            "substrate" => filter.kinds = vec![SubstrateKind::Note],
            "short_kind" => filter.record_kinds = vec!["ai".to_string()],
            _ => unreachable!(),
        }
        let hits = store
            .search_with_options(
                TextSearchRequest {
                    query: "zircon".to_string(),
                    filter: Some(filter),
                    ..request(2, 64)
                },
                options(Some(2)),
            )
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "{case}: a gathered UUID must not expand into another physical row"
        );
        assert_eq!(hits[0].subject_id, id);
        assert_eq!(hits[0].title.as_deref(), Some("local title"));
        assert_eq!(hits[0].snippet.as_deref(), Some("zircon local evidence"));
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].score, DeterministicScore::from_f64(1.0));
    }
}

#[tokio::test]
async fn bounded_ranking_applies_all_exact_filters_before_the_cap() {
    let store = setup_trigram_store("bounded_filters_first");
    let wanted = Uuid::from_u128(90);
    for i in 0..10u128 {
        // These match every other predicate: only the explicit ID filter can
        // prevent them from exhausting the first-row gather before `wanted`.
        let doc =
            make_record_kind_document(Uuid::from_u128(i + 1), "memory", "noise", "needle noise");
        store.upsert_document(doc).await.unwrap();
    }
    store
        .upsert_document(make_record_kind_document(
            wanted,
            "memory",
            "wanted",
            "needle wanted",
        ))
        .await
        .unwrap();
    let req = TextSearchRequest {
        filter: Some(TextFilter {
            ids: vec![wanted],
            namespaces: vec!["test_ns".to_string()],
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec!["memory".to_string()],
        }),
        ..request(1, 64)
    };
    let hits = store
        .search_with_options(req, options(Some(1)))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, wanted);
    assert_eq!(hits[0].title.as_deref(), Some("wanted"));
}

#[tokio::test]
async fn bounded_ranking_counts_only_issued_statements_for_empty_and_missing_queries() {
    let store = setup_memory_store("bounded_empty_usage");
    store
        .upsert_document(make_document(
            Uuid::from_u128(1),
            "positive",
            "needle evidence",
        ))
        .await
        .unwrap();
    for (query, expected_count, expected_hits) in [
        ("", None, 0),
        ("\"\"^^**", None, 0),
        ("absenttoken", Some(1), 0),
        ("needle", Some(1), 1),
    ] {
        let context = khive_storage::usage::UsageContext::new();
        let hits = khive_storage::usage::scope(
            context.clone(),
            store.search_with_options(
                TextSearchRequest {
                    query: query.to_string(),
                    ..request(2, 0)
                },
                options(Some(3)),
            ),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), expected_hits, "query={query:?}");
        assert_eq!(
            context
                .snapshot()
                .get("fts_passes")
                .and_then(serde_json::Value::as_u64),
            expected_count,
            "query={query:?}"
        );
    }
}

/// Run identical source on the namespace-corrected two-cursor implementation
/// and the materialized implementation. Seeding and result comparisons are
/// outside each measured actual TextSearch API call; no timing threshold is
/// asserted. Every printed result payload must match across the two runs.
#[tokio::test]
#[ignore = "deterministic FTS benchmark; run explicitly with --ignored --exact --nocapture"]
async fn benchmark_rank_within_cap_actual_api() {
    const ROWS: usize = 10_000;
    const REPETITIONS: usize = 21;
    const TOP_K: u32 = 20;
    let store = setup_trigram_store("bounded_ranking_benchmark");
    let terms = ["knowledge", "graph", "memory", "recall"];
    let documents = (0..ROWS)
        .map(|i| {
            make_record_kind_document(
                Uuid::from_u128(i as u128 + 1),
                "memory",
                &format!("fixture {i}"),
                &format!(
                    "{} {} {}",
                    format!("{} ", terms[i % terms.len()]).repeat(1 + i % 11),
                    terms[(i + 1) % terms.len()],
                    "padding ".repeat(i % 37)
                ),
            )
        })
        .collect();
    let seeded = store.upsert_documents(documents).await.unwrap();
    assert_eq!(seeded.affected, ROWS as u64);
    assert_eq!(seeded.failed, 0);
    assert_eq!(
        store.count(ns_filter("test_ns")).await.unwrap(),
        ROWS as u64
    );

    for gather_limit in [50, 200, 500] {
        for snippet_chars in [0, 12] {
            let mut req = request(TOP_K, snippet_chars);
            req.query = terms.join(" ");
            req.mode = TextQueryMode::AnyTerm;
            req.filter.as_mut().unwrap().record_kinds = vec!["memory".to_string()];
            let opts = options(Some(gather_limit));
            let warm = store
                .search_with_options(req.clone(), opts.clone())
                .await
                .unwrap();
            assert_eq!(warm.len(), TOP_K as usize);
            assert!(warm.iter().all(|hit| {
                hit.subject_id.as_u128() <= u128::from(gather_limit)
                    && hit.snippet.is_none() == (snippet_chars == 0)
            }));
            let expected = serde_json::to_value(&warm).unwrap();
            let mut timings = Vec::with_capacity(REPETITIONS);
            for _ in 0..REPETITIONS {
                let request = req.clone();
                let options = opts.clone();
                let started = std::time::Instant::now();
                let hits = store.search_with_options(request, options).await.unwrap();
                let elapsed = started.elapsed();
                assert_eq!(hits.len(), TOP_K as usize);
                assert_eq!(serde_json::to_value(&hits).unwrap(), expected);
                timings.push(u64::try_from(elapsed.as_nanos()).unwrap());
            }
            timings.sort_unstable();
            println!(
                "{}",
                serde_json::json!({
                    "benchmark": "fts_rank_within_cap_actual_api",
                    "sqlite_version": rusqlite::version(),
                    "rows": ROWS,
                    "gather_limit": gather_limit,
                    "top_k": TOP_K,
                    "snippet_chars": snippet_chars,
                    "repetitions": REPETITIONS,
                    "median_ns": timings[REPETITIONS / 2],
                    "sorted_samples_ns": timings,
                    "result": expected,
                })
            );
        }
    }
}
