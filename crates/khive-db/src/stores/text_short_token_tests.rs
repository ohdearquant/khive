use super::*;
use crate::pool::PoolConfig;

fn fixture() -> Fts5TextSearch {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            max_readers: 0,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE fts_short_contract USING fts5(\
             subject_id UNINDEXED, kind UNINDEXED, title, body, tags UNINDEXED, \
             namespace UNINDEXED, metadata UNINDEXED, updated_at UNINDEXED, \
             record_kind, tokenize = 'trigram')",
            )
            .unwrap();
        writer
            .conn()
            .execute_batch(&rowid_map_ddl("fts_short_contract"))
            .unwrap();
    }
    Fts5TextSearch::new(pool, false, "short_contract".into())
}

fn document(n: u128, title: &str, body: &str, namespace: &str) -> TextDocument {
    TextDocument {
        subject_id: Uuid::from_u128(n),
        kind: SubstrateKind::Note,
        record_kind: Some("memory".into()),
        namespace: namespace.into(),
        title: (!title.is_empty()).then(|| title.into()),
        body: body.into(),
        tags: vec![],
        metadata: None,
        updated_at: Utc.timestamp_opt(1_700_000_000, 0).single().unwrap(),
    }
}

async fn seed(store: &Fts5TextSearch, documents: Vec<TextDocument>) {
    for doc in documents {
        store.upsert_document(doc).await.unwrap();
    }
}

fn request(query: &str, top_k: u32) -> TextSearchRequest {
    TextSearchRequest {
        query: query.into(),
        mode: TextQueryMode::AnyTerm,
        filter: Some(TextFilter {
            namespaces: vec!["local".into()],
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec!["memory".into()],
            ..TextFilter::default()
        }),
        top_k,
        snippet_chars: 48,
    }
}

fn modes(gather_limit: u32) -> [TextSearchOptions; 3] {
    [
        TextGatherMode::Ranked,
        TextGatherMode::Unranked,
        TextGatherMode::RankWithinCap,
    ]
    .map(|gather_mode| TextSearchOptions {
        gather_mode,
        gather_limit: Some(gather_limit),
    })
}

#[derive(Clone, Debug)]
struct LegacyRow {
    rowid: i64,
    id: Uuid,
    bm25: f64,
    title: String,
    snippet: Option<String>,
}

// Execute the pre-change cursor shapes, independent of the short-token implementation.
// The extra physical rowid is evidence only; it cannot alter the old ordering or limits.
fn legacy_rows(
    store: &Fts5TextSearch,
    req: &TextSearchRequest,
    options: &TextSearchOptions,
    outer_limit: u32,
) -> Vec<LegacyRow> {
    let Some(expr) = build_filtered_match_expr(&req.query, req.mode.clone(), req.filter.as_ref())
    else {
        return vec![];
    };
    let capped = options.gather_mode == TextGatherMode::RankWithinCap;
    let unranked = options.gather_mode == TextGatherMode::Unranked;
    let table = &store.table_name;
    let (filter, params) = req
        .filter
        .as_ref()
        .map(|f| build_filter_clause(f, table, if capped { 4 } else { 3 }))
        .unwrap_or_default();
    let snippet = if unranked || req.snippet_chars == 0 {
        "NULL AS snippet".into()
    } else {
        format!(
            "snippet({table}, 3, '', '', '...', {}) AS snippet",
            i32::try_from(req.snippet_chars).unwrap_or(i32::MAX)
        )
    };
    let sql = if unranked {
        format!(
            "SELECT rowid, subject_id, 0.0, title, {snippet} FROM {table} \
                 WHERE {table} MATCH ?1{filter} LIMIT ?2"
        )
    } else if capped {
        format!(
            "WITH gathered AS MATERIALIZED (SELECT rowid AS physical_row, subject_id, \
                 rank AS raw_rank, title, {snippet} FROM {table} WHERE {table} MATCH ?1 \
                 AND rank MATCH '{LEXICAL_BM25_RANK}'{filter} LIMIT ?2) \
                 SELECT physical_row, subject_id, raw_rank, title, snippet FROM gathered \
                 ORDER BY raw_rank LIMIT ?3"
        )
    } else {
        format!(
            "SELECT rowid, subject_id, rank, title, {snippet} FROM {table} \
                 WHERE {table} MATCH ?1 AND rank MATCH '{LEXICAL_BM25_RANK}'{filter} \
                 ORDER BY rank LIMIT ?2"
        )
    };
    let writer = store.pool.writer().unwrap();
    let mut statement = writer.conn().prepare(&sql).unwrap();
    statement.raw_bind_parameter(1, expr).unwrap();
    let bound = if capped {
        options.gather_limit.unwrap_or(req.top_k).max(req.top_k)
    } else {
        outer_limit
    };
    statement.raw_bind_parameter(2, i64::from(bound)).unwrap();
    if capped {
        statement
            .raw_bind_parameter(3, i64::from(outer_limit))
            .unwrap();
    }
    for (i, param) in params.iter().enumerate() {
        statement
            .raw_bind_parameter((if capped { 4 } else { 3 }) + i, param.to_sql().unwrap())
            .unwrap();
    }
    let mut cursor = statement.raw_query();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().unwrap() {
        rows.push(LegacyRow {
            rowid: row.get(0).unwrap(),
            id: row.get::<_, String>(1).unwrap().parse().unwrap(),
            bm25: row.get(2).unwrap(),
            title: row.get(3).unwrap(),
            snippet: row.get(4).unwrap(),
        });
    }
    rows
}

fn legacy_hits(rows: &[LegacyRow], mode: TextGatherMode) -> Vec<TextSearchHit> {
    let lo = rows.iter().map(|r| r.bm25).fold(f64::INFINITY, f64::min);
    let hi = rows
        .iter()
        .map(|r| r.bm25)
        .fold(f64::NEG_INFINITY, f64::max);
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let value = if mode == TextGatherMode::Unranked || (hi - lo).abs() < 1e-12 {
                1.0
            } else {
                0.05 + 0.95 * ((hi - r.bm25) / (hi - lo))
            };
            TextSearchHit {
                subject_id: r.id,
                score: DeterministicScore::from_f64(value),
                rank: (i + 1) as u32,
                title: (!r.title.is_empty()).then(|| r.title.clone()),
                snippet: r.snippet.clone().filter(|s| !s.is_empty()),
            }
        })
        .collect()
}

fn ids(hits: &[TextSearchHit]) -> Vec<Uuid> {
    hits.iter().map(|h| h.subject_id).collect()
}

fn assert_identity(actual: &[TextSearchHit], expected: &[TextSearchHit]) {
    assert_eq!(
        serde_json::to_vec(actual).unwrap(),
        serde_json::to_vec(expected).unwrap()
    );
}

fn assert_fields_and_scores(hits: &[TextSearchHit], pool: &[LegacyRow], mode: TextGatherMode) {
    let selected: Vec<_> = hits
        .iter()
        .map(|hit| {
            pool.iter()
                .find(|row| {
                    row.id == hit.subject_id
                        && (!row.title.is_empty()).then(|| row.title.clone()) == hit.title
                })
                .expect("every selected physical row must belong to the unchanged candidate window")
                .clone()
        })
        .collect();
    let expected = legacy_hits(&selected, mode);
    let mut old_scores: Vec<_> = expected
        .iter()
        .map(|h| h.score.to_f64().to_bits())
        .collect();
    let mut new_scores: Vec<_> = hits.iter().map(|h| h.score.to_f64().to_bits()).collect();
    old_scores.sort_unstable();
    new_scores.sort_unstable();
    assert_eq!(
        new_scores, old_scores,
        "preserve old normalization over exactly the final selected K"
    );
    for (i, (hit, old)) in hits.iter().zip(&expected).enumerate() {
        assert_eq!(hit.rank, (i + 1) as u32, "rank is the returned ordinal");
        assert_eq!(hit.title, old.title);
        assert_eq!(hit.snippet, old.snippet);
    }
    assert!(
        hits.windows(2)
            .all(|w| w[0].score.to_f64() >= w[1].score.to_f64()),
        "returned scores must descend with returned order"
    );
}

#[tokio::test]
async fn type_two_moves_without_filtering_in_all_gather_modes() {
    let store = fixture();
    seed(
        &store,
        vec![
            document(
                1,
                "diabetes",
                "type 1 diabetes with chronic kidney disease",
                "local",
            ),
            document(
                2,
                "",
                "type 2 diabetes with chronic kidney disease",
                "local",
            ),
            document(3, "", "diabetes kidney reference", "local"),
        ],
    )
    .await;
    for query_mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        for options in modes(3) {
            let mut req = request("type 2 diabetes with chronic kidney disease", 2);
            req.mode = query_mode.clone();
            let old = legacy_rows(&store, &req, &options, 2);
            assert_eq!(
                old[0].id,
                Uuid::from_u128(1),
                "fixture must expose the old type1 preference"
            );
            let observed = legacy_rows(
                &store,
                &req,
                &options,
                if options.gather_mode == TextGatherMode::Unranked {
                    2
                } else {
                    4
                },
            );
            let hits = store
                .search_with_options(req.clone(), options.clone())
                .await
                .unwrap();
            assert_eq!(ids(&hits), vec![Uuid::from_u128(2), Uuid::from_u128(1)]);
            assert_fields_and_scores(&hits, &observed, options.gather_mode);
            if options.gather_mode != TextGatherMode::Unranked {
                let old = legacy_hits(&old, options.gather_mode);
                assert_ne!(
                    old[0].score, old[1].score,
                    "nonuniform score reassignment witness"
                );
                assert_eq!(hits[0].score, old[0].score);
            }
            if options.gather_mode == TextGatherMode::Ranked {
                assert_identity(&store.search(req).await.unwrap(), &hits);
            }
        }
    }
}

#[tokio::test]
async fn whole_ascii_signals_are_distinct_case_folded_and_stably_tied() {
    let store = fixture();
    seed(
        &store,
        vec![
            document(1, "anchor", "v20 KValue type2", "local"),
            document(2, "anchor", "V2 kv", "local"),
            document(3, "anchor v2 KV", "v2 v2 KV KV v2", "local"),
            document(4, "anchor", "v2v2 (Kv)", "local"),
            document(5, "anchor", "v2_v2", "local"),
            document(6, "anchor", "界v2界", "local"),
        ],
    )
    .await;
    for options in modes(6) {
        let req = request("anchor v2 KV v2", 6);
        let old = legacy_rows(&store, &req, &options, 6);
        assert_eq!(old.len(), 6);
        assert!(
            old.iter().position(|r| r.id == Uuid::from_u128(2)).unwrap()
                < old.iter().position(|r| r.id == Uuid::from_u128(3)).unwrap(),
            "duplicate occurrence witness starts behind the equal-count row"
        );
        let mut expected = Vec::new();
        for group in [&[2_u128, 3][..], &[4, 5, 6][..], &[1][..]] {
            expected.extend(
                old.iter()
                    .filter(|r| group.iter().any(|n| r.id == Uuid::from_u128(*n)))
                    .map(|r| r.id),
            );
        }
        let hits = store
            .search_with_options(req, options.clone())
            .await
            .unwrap();
        assert_eq!(
            ids(&hits),
            expected,
            "whole runs, one point per distinct token across both fields, stable equal counts"
        );
        assert_fields_and_scores(&hits, &old, options.gather_mode);
    }
}

#[tokio::test]
async fn common_shortword_dilution_preserves_the_equal_count_limit() {
    let store = fixture();
    seed(
        &store,
        vec![
            document(
                1,
                "diabetes",
                "type 1 of diabetes with chronic kidney disease",
                "local",
            ),
            document(
                2,
                "",
                "type 2 diabetes with chronic kidney disease",
                "local",
            ),
        ],
    )
    .await;
    for options in modes(2) {
        let req = request("type 2 of diabetes with chronic kidney disease", 2);
        let old = legacy_rows(&store, &req, &options, 2);
        assert_eq!(old[0].id, Uuid::from_u128(1));
        let hits = store
            .search_with_options(req, options.clone())
            .await
            .unwrap();
        assert_identity(&hits, &legacy_hits(&old, options.gather_mode));
        assert_eq!(
            ids(&hits),
            vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            "of and 2 each contribute one point; no implicit numeral priority"
        );
    }
}

#[tokio::test]
async fn no_signal_and_zero_move_paths_preserve_complete_legacy_hits() {
    let store = fixture();
    seed(
        &store,
        vec![
            document(
                1,
                "diabetes",
                "type 1 diabetes disease 2026-10-25 糖尿病 kidney",
                "local",
            ),
            document(2, "", "type 2 diabetes kidney disease", "local"),
            document(3, "", "GQA reference without cache identifiers", "local"),
        ],
    )
    .await;
    for (query, mode) in [
        ("diabetes disease", TextQueryMode::Plain),
        ("diabetes z9", TextQueryMode::AnyTerm),
        ("2026-10-25 diabetes", TextQueryMode::AnyTerm),
        ("diabetes OR disease", TextQueryMode::Plain),
        ("\"v2\" diabetes", TextQueryMode::AnyTerm),
        ("type 2 diabetes", TextQueryMode::Phrase),
        ("糖尿病 kidney", TextQueryMode::AnyTerm),
        ("GQA KV", TextQueryMode::AnyTerm),
    ] {
        for options in modes(2) {
            let mut req = request(query, 2);
            req.mode = mode.clone();
            let old = legacy_rows(&store, &req, &options, 2);
            assert!(!old.is_empty(), "nonvacuous legacy witness for {query}");
            let hits = store
                .search_with_options(req, options.clone())
                .await
                .unwrap();
            assert_identity(&hits, &legacy_hits(&old, options.gather_mode));
        }
    }
}

#[tokio::test]
async fn ranked_budget_caps_extra_rows_but_unranked_keeps_its_entire_tail() {
    let store = fixture();
    seed(
        &store,
        (1..=144)
            .map(|n| {
                document(
                    n,
                    "",
                    if [4, 5, 129, 130, 144].contains(&n) {
                        "anchor 2"
                    } else {
                        "anchor 1"
                    },
                    "local",
                )
            })
            .collect(),
    )
    .await;
    for mode in [TextGatherMode::Ranked, TextGatherMode::RankWithinCap] {
        let options = TextSearchOptions {
            gather_mode: mode,
            gather_limit: Some(144),
        };
        for (k, budget) in [(2, 4), (65, 129)] {
            let req = request("anchor 2", k);
            let old = legacy_rows(&store, &req, &options, budget);
            assert_eq!(old.len(), budget as usize);
            assert_eq!(
                old.last().unwrap().rowid,
                i64::from(budget),
                "fixture pins the original equal-BM25 physical cursor"
            );
            let hits = store
                .search_with_options(req, options.clone())
                .await
                .unwrap();
            assert_eq!(hits.len(), k as usize);
            assert_eq!(hits[0].subject_id, Uuid::from_u128(4));
            assert!(
                !ids(&hits).contains(&Uuid::from_u128(if k == 2 { 5 } else { 130 })),
                "never refill beyond B"
            );
            if k == 65 {
                assert!(ids(&hits).contains(&Uuid::from_u128(129)));
            }
            assert_fields_and_scores(&hits, &old, mode);
        }
    }
    for k in [2, 144] {
        let options = TextSearchOptions {
            gather_mode: TextGatherMode::Unranked,
            gather_limit: Some(144),
        };
        let req = request("anchor 2", k);
        let old = legacy_rows(&store, &req, &options, k);
        let hits = store.search_with_options(req, options).await.unwrap();
        if k == 2 {
            assert_identity(&hits, &legacy_hits(&old, TextGatherMode::Unranked));
        } else {
            assert!(
                ids(&hits)[..5].contains(&Uuid::from_u128(144)),
                "rerank all legacy K rows, including the tail beyond 64/128"
            );
        }
        assert_fields_and_scores(&hits, &old, TextGatherMode::Unranked);
    }
}

#[tokio::test]
async fn filters_and_gather_eligibility_stay_before_ranking_and_limits() {
    let store = fixture();
    let mut wrong_kind = document(2, "", "anchor 2 v2", "local");
    wrong_kind.record_kind = Some("task".into());
    seed(
        &store,
        vec![
            document(1, "", "anchor 2 v2", "foreign"),
            wrong_kind,
            document(3, "", "anchor 1", "local"),
            document(4, "", "anchor 2", "local"),
            document(5, "", "anchor 2 v2", "local"),
        ],
    )
    .await;
    let req = request("anchor 2 v2", 1);
    for (gather, expected) in [(0, 3), (1, 3), (2, 4)] {
        let options = TextSearchOptions {
            gather_mode: TextGatherMode::RankWithinCap,
            gather_limit: Some(gather),
        };
        let hits = store
            .search_with_options(req.clone(), options.clone())
            .await
            .unwrap();
        assert_eq!(
            ids(&hits),
            vec![Uuid::from_u128(expected)],
            "G=max(gather,K), filters before G, no outside promotion"
        );
        let raw = legacy_rows(&store, &req, &options, 2);
        assert_fields_and_scores(&hits, &raw, options.gather_mode);
    }
    for k in [0, u32::MAX] {
        for options in modes(5) {
            let req = request("anchor 2 v2", k);
            let hits = store
                .search_with_options(req.clone(), options.clone())
                .await
                .unwrap();
            if k == 0 {
                assert!(hits.is_empty());
            } else {
                assert_eq!(hits.len(), 3, "saturating budget on a tiny filtered corpus");
                let raw = legacy_rows(&store, &req, &options, u32::MAX);
                assert_fields_and_scores(&hits, &raw, options.gather_mode);
            }
        }
    }
}

#[tokio::test]
async fn body_observations_use_the_selected_physical_row_and_namespace() {
    let store = fixture();
    seed(
        &store,
        vec![
            document(1, "local first", "anchor 1", "local"),
            document(1, "foreign same id", "anchor 2 v2", "foreign"),
            document(2, "local second", "anchor 2", "local"),
        ],
    )
    .await;
    for options in modes(3) {
        let req = request("anchor 2 v2", 2);
        let raw = legacy_rows(&store, &req, &options, 4);
        assert_eq!(raw.len(), 2);
        let hits = store
            .search_with_options(req, options.clone())
            .await
            .unwrap();
        assert_eq!(
            ids(&hits),
            vec![Uuid::from_u128(2), Uuid::from_u128(1)],
            "foreign body must not lend its second signal to the selected local row"
        );
        assert_eq!(hits[1].title.as_deref(), Some("local first"));
        assert_fields_and_scores(&hits, &raw, options.gather_mode);
        let mut req = request("anchor 2 v2", 3);
        req.filter
            .as_mut()
            .unwrap()
            .namespaces
            .push("foreign".into());
        let hits = store.search_with_options(req, options).await.unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter()
                .filter(|h| h.subject_id == Uuid::from_u128(1))
                .count(),
            2,
            "do not deduplicate physical rows by subject id"
        );
    }
}

#[tokio::test]
async fn score_distribution_uses_the_final_selected_k_not_discarded_overfetch_rows() {
    let store = fixture();
    seed(
        &store,
        (1..=6)
            .map(|n| {
                document(
                    n,
                    "",
                    &format!(
                        "anchor {} {}",
                        if [3, 4].contains(&n) { 2 } else { 1 },
                        "padding ".repeat(n as usize * 10)
                    ),
                    "local",
                )
            })
            .collect(),
    )
    .await;
    for options in [modes(6)[0].clone(), modes(6)[2].clone()] {
        let req = request("anchor 2", 3);
        let observed = legacy_rows(&store, &req, &options, 6);
        assert_eq!(
            observed.iter().map(|r| r.id).collect::<Vec<_>>(),
            (1..=6).map(Uuid::from_u128).collect::<Vec<_>>(),
            "fixture exposes six distinct BM25 positions"
        );
        let hits = store
            .search_with_options(req, options.clone())
            .await
            .unwrap();
        assert_eq!(
            ids(&hits),
            vec![Uuid::from_u128(3), Uuid::from_u128(4), Uuid::from_u128(1)]
        );
        assert!(
            observed[3].bm25 < observed[5].bm25,
            "discarded tail must have a different normalization maximum"
        );
        assert_fields_and_scores(&hits, &observed, options.gather_mode);
        assert!(
            hits[0].score.to_f64() > hits[1].score.to_f64()
                && hits[1].score.to_f64() > hits[2].score.to_f64(),
            "three distinct final-K scores witness the distribution rather than only endpoints"
        );
    }
}

#[tokio::test]
async fn reordering_only_the_overfetch_tail_keeps_returned_hits_byte_identical() {
    let store = fixture();
    seed(
        &store,
        (1..=6)
            .map(|n| {
                document(
                    n,
                    "",
                    &format!(
                        "anchor {} {}",
                        if [1, 2, 3, 5].contains(&n) { 2 } else { 1 },
                        "padding ".repeat(n as usize * 10)
                    ),
                    "local",
                )
            })
            .collect(),
    )
    .await;
    for options in [modes(6)[0].clone(), modes(6)[2].clone()] {
        let req = request("anchor 2", 3);
        let old = legacy_rows(&store, &req, &options, 3);
        assert_eq!(
            old.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
        );
        let observed = legacy_rows(&store, &req, &options, 6);
        assert_eq!(observed[3].id, Uuid::from_u128(4));
        assert_eq!(observed[4].id, Uuid::from_u128(5));
        let hits = store
            .search_with_options(req, options.clone())
            .await
            .unwrap();
        assert_identity(&hits, &legacy_hits(&old, options.gather_mode));
    }
}

#[tokio::test]
async fn unordered_metadata_traversal_cannot_replace_the_original_cursor_order() {
    let store = fixture();
    seed(
        &store,
        (1..=6)
            .map(|n| {
                document(
                    n,
                    "",
                    &format!("anchor {}", "padding ".repeat(n as usize)),
                    "local",
                )
            })
            .collect(),
    )
    .await;
    for reverse in [false, true] {
        {
            let writer = store.pool.writer().unwrap();
            writer
                .conn()
                .execute_batch(if reverse {
                    "PRAGMA reverse_unordered_selects = ON"
                } else {
                    "PRAGMA reverse_unordered_selects = OFF"
                })
                .unwrap();
        }
        // max_readers=0 makes both the independent cursor and the typed search
        // use this same connection, so the connection-local setting is real.
        for options in modes(6) {
            for query in ["anchor", "anchor z9"] {
                let req = request(query, 3);
                let raw = legacy_rows(&store, &req, &options, 3);
                assert_eq!(raw.len(), 3);
                let hits = store
                    .search_with_options(req, options.clone())
                    .await
                    .unwrap();
                assert_identity(&hits, &legacy_hits(&raw, options.gather_mode));
            }
        }
    }

    // Equal indexed fields make the ranked and capped source window genuinely
    // tied; only the unindexed UUID differs. The earlier corpus keeps its
    // non-tied BM25 witness as a separate check.
    let tied_store = fixture();
    seed(
        &tied_store,
        (1..=6)
            .map(|n| document(n, "", "anchor padding", "local"))
            .collect(),
    )
    .await;
    for reverse in [false, true] {
        {
            let writer = tied_store.pool.writer().unwrap();
            writer
                .conn()
                .execute_batch(if reverse {
                    "PRAGMA reverse_unordered_selects = ON"
                } else {
                    "PRAGMA reverse_unordered_selects = OFF"
                })
                .unwrap();
        }
        for options in modes(6) {
            for query in ["anchor", "anchor z9"] {
                let req = request(query, 3);
                let raw = legacy_rows(&tied_store, &req, &options, 3);
                assert_eq!(raw.len(), 3);
                assert!(
                    raw.iter()
                        .all(|row| row.bm25.to_bits() == raw[0].bm25.to_bits()),
                    "the second corpus must expose tied source ranks"
                );
                let hits = tied_store
                    .search_with_options(req, options.clone())
                    .await
                    .unwrap();
                assert_identity(&hits, &legacy_hits(&raw, options.gather_mode));
            }
        }
    }
}
