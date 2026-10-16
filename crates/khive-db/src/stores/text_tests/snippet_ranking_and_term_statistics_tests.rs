/// snippet_chars=0 omits snippet computation without changing IDs, ranks, or scores.
///
/// Regression for the snippet-free FTS optimization: verifies the `NULL AS snippet`
/// path returns identical candidate identity and ordering to the regular path, and
/// that every snippet field is None when snippet_chars=0.
#[tokio::test]
async fn search_snippet_chars_zero_omits_snippets_without_changing_rank() {
    let store = setup_memory_store("snippet_zero");

    let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
    let bodies = [
        "alpha bravo charlie delta the quick fox jumped",
        "bravo charlie delta echo the slow fox slept",
        "charlie delta echo foxtrot the lazy dog barked",
        "delta echo foxtrot golf a completely different document",
    ];
    for (id, body) in ids.iter().zip(bodies.iter()) {
        store
            .upsert_document(make_document(*id, "title", body))
            .await
            .unwrap();
    }

    let req_with = TextSearchRequest {
        query: "bravo charlie".to_string(),
        mode: TextQueryMode::AnyTerm,
        filter: Some(ns_filter("test_ns")),
        top_k: 10,
        snippet_chars: 64,
    };
    let req_zero = TextSearchRequest {
        snippet_chars: 0,
        ..req_with.clone()
    };

    let hits_with = store.search(req_with).await.unwrap();
    let hits_zero = store.search(req_zero).await.unwrap();

    assert!(!hits_with.is_empty(), "snippet path must return hits");
    assert_eq!(
        hits_with.len(),
        hits_zero.len(),
        "hit count must be identical regardless of snippet_chars"
    );

    for (hw, hz) in hits_with.iter().zip(hits_zero.iter()) {
        assert_eq!(hw.subject_id, hz.subject_id, "subject_id must match");
        assert_eq!(hw.rank, hz.rank, "rank must match");
        assert!(
            (hw.score.to_f64() - hz.score.to_f64()).abs() < 1e-12,
            "score must match: with={} zero={}",
            hw.score.to_f64(),
            hz.score.to_f64()
        );
        assert!(
            hz.snippet.is_none(),
            "snippet must be None when snippet_chars=0, got {:?}",
            hz.snippet
        );
    }
}

// Boundary case: a hit ranked near the last position in a multi-result set
// must still have snippet=None when snippet_chars=0.
#[tokio::test]
async fn search_snippet_chars_zero_bottom_ranked_hit_has_no_snippet() {
    let store = setup_memory_store("snippet_zero_boundary");

    // Insert enough docs so the last-ranked result is a "boundary" case.
    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    for (i, id) in ids.iter().enumerate() {
        let body = format!("keyword_boundary doc number {i} with varying relevance");
        store
            .upsert_document(make_document(*id, "t", &body))
            .await
            .unwrap();
    }

    let hits = store
        .search(TextSearchRequest {
            query: "keyword_boundary".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 0,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 5, "all 5 docs must match");
    // The last-ranked hit (boundary) must also have no snippet.
    let last = hits.last().unwrap();
    assert!(
        last.snippet.is_none(),
        "bottom-ranked hit must have snippet=None when snippet_chars=0, got {:?}",
        last.snippet
    );
}

/// Score normalization: all scores stay in (0, 1], and a single-hit result
/// scores ≈ 1.0. This validates the normalization formula independent of
/// FTS5 rank ordering guarantees (which are already tested via `rank` field).
#[tokio::test]
async fn test_score_normalization_range() {
    let store = setup_memory_store("score_range");

    // Insert three documents; only two match the query.
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let id3 = Uuid::new_v4();
    store
        .upsert_document(make_document(
            id1,
            "normtest topic",
            "normtest normtest normtest",
        ))
        .await
        .unwrap();
    store
        .upsert_document(make_document(
            id2,
            "normtest light",
            "other content without the keyword",
        ))
        .await
        .unwrap();
    store
        .upsert_document(make_document(
            id3,
            "irrelevant title",
            "completely different document content",
        ))
        .await
        .unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "normtest".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    // id3 must not match; id1 and id2 should.
    assert!(!hits.is_empty(), "at least one doc must match");
    assert!(
        hits.iter().all(|h| h.subject_id != id3),
        "id3 must not appear"
    );

    // All scores must be in (0, 1].
    for h in &hits {
        let s = h.score.to_f64();
        assert!(s > 0.0 && s <= 1.0, "score out of (0,1]: {s}");
    }
    // Rank field must be 1-indexed and contiguous.
    for (i, h) in hits.iter().enumerate() {
        assert_eq!(h.rank, (i + 1) as u32, "rank must equal position+1");
    }
    // Best hit (rank=1) must score ≈ 1.0 — normalization anchors the best
    // rank to 1.0 regardless of absolute BM25 magnitude.
    assert!(
        hits[0].score.to_f64() > 0.99,
        "top hit must score ≈ 1.0, got {}",
        hits[0].score.to_f64()
    );

    // Single-hit result: the only match scores ≈ 1.0 (degenerate case:
    // range == 0 → all hits get 1.0).
    let single_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            single_id,
            "xqzplurp_unique_marker",
            "xqzplurp_unique_marker body",
        ))
        .await
        .unwrap();
    let single = store
        .search(TextSearchRequest {
            query: "xqzplurp_unique_marker".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();
    assert_eq!(single.len(), 1);
    assert!(
        single[0].score.to_f64() > 0.99,
        "single-hit must score ≈ 1.0, got {}",
        single[0].score.to_f64()
    );
}

// ── search_with_options tests ─────────────────────────────────────────────

#[tokio::test]
async fn search_with_options_default_matches_search() {
    let store = setup_memory_store("opts_default");

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    store
        .upsert_document(make_document(id1, "alpha beta", "alpha beta gamma"))
        .await
        .unwrap();
    store
        .upsert_document(make_document(id2, "delta epsilon", "delta epsilon zeta"))
        .await
        .unwrap();

    let req = TextSearchRequest {
        query: "alpha".to_string(),
        mode: TextQueryMode::Plain,
        filter: Some(ns_filter("test_ns")),
        top_k: 10,
        snippet_chars: 0,
    };

    let plain = store.search(req.clone()).await.unwrap();
    let with_opts = store
        .search_with_options(req, TextSearchOptions::default())
        .await
        .unwrap();

    assert_eq!(
        plain.len(),
        with_opts.len(),
        "default options must match plain search"
    );
    for (p, w) in plain.iter().zip(with_opts.iter()) {
        assert_eq!(p.subject_id, w.subject_id);
        assert_eq!(p.rank, w.rank);
    }
}

#[tokio::test]
async fn search_unranked_returns_capped_candidates() {
    let store = setup_memory_store("unranked_cap");

    for i in 0..10u32 {
        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                &format!("doc {i}"),
                &format!("keyword content {i}"),
            ))
            .await
            .unwrap();
    }

    let hits = store
        .search_with_options(
            TextSearchRequest {
                query: "keyword".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 5,
                snippet_chars: 0,
            },
            TextSearchOptions {
                gather_mode: khive_storage::types::TextGatherMode::Unranked,
                gather_limit: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(hits.len(), 5, "unranked must cap at top_k");
    for h in &hits {
        assert!(
            (h.score.to_f64() - 1.0).abs() < 1e-10,
            "unranked hits must have uniform score 1.0, got {}",
            h.score.to_f64()
        );
        assert!(
            h.snippet.is_none(),
            "unranked with snippet_chars=0 must have no snippet"
        );
    }
}

#[tokio::test]
async fn search_rank_within_cap_returns_ranked_subset() {
    let store = setup_memory_store("rank_within_cap");

    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    let bodies = [
        "rust programming language systems",
        "rust systems memory safety",
        "programming language design patterns",
        "memory management allocation",
        "systems software engineering",
    ];
    for (id, body) in ids.iter().zip(bodies.iter()) {
        store
            .upsert_document(make_document(*id, "doc", body))
            .await
            .unwrap();
    }

    let hits = store
        .search_with_options(
            TextSearchRequest {
                query: "rust".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 3,
                snippet_chars: 0,
            },
            TextSearchOptions {
                gather_mode: khive_storage::types::TextGatherMode::RankWithinCap,
                gather_limit: Some(10),
            },
        )
        .await
        .unwrap();

    // Must return at most top_k (3) hits with BM25-normalized scores.
    assert!(hits.len() <= 3, "rank_within_cap must cap at top_k");
    assert!(!hits.is_empty(), "must find at least one 'rust' hit");
    for h in &hits {
        let score = h.score.to_f64();
        assert!(score > 0.0 && score <= 1.0, "scores must be in (0, 1]");
    }
    // Ranks must be 1-indexed and contiguous.
    for (i, h) in hits.iter().enumerate() {
        assert_eq!(h.rank, (i + 1) as u32, "rank must equal position+1");
    }
}

// ── term_stats tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn term_stats_returns_df_and_idf_for_fixture() {
    let store = setup_memory_store("term_stats_fixture");

    // Insert 10 docs: 3 contain "rare_term", 8 contain "common_term".
    for i in 0..8u32 {
        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                &format!("doc {i}"),
                &format!("common_term content number {i}"),
            ))
            .await
            .unwrap();
    }
    for i in 0..3u32 {
        store
            .upsert_document(make_document(
                Uuid::new_v4(),
                &format!("rare {i}"),
                &format!("rare_term common_term extra {i}"),
            ))
            .await
            .unwrap();
    }

    let stats = store
        .term_stats(TextTermStatsRequest {
            terms: vec!["rare_term".to_string(), "common_term".to_string()],
            filter: Some(ns_filter("test_ns")),
        })
        .await
        .unwrap();

    assert_eq!(stats.len(), 2);
    let rare = stats.iter().find(|s| s.term == "rare_term").unwrap();
    let common = stats.iter().find(|s| s.term == "common_term").unwrap();

    assert_eq!(rare.document_count, 11, "total doc count must be 11");
    assert_eq!(rare.document_frequency, 3, "rare_term appears in 3 docs");
    assert_eq!(
        common.document_frequency, 11,
        "common_term appears in all 11 docs"
    );
    assert!(
        rare.inverse_document_frequency > common.inverse_document_frequency,
        "rarer term must have higher IDF: rare={} common={}",
        rare.inverse_document_frequency,
        common.inverse_document_frequency
    );
}

#[tokio::test]
async fn term_stats_empty_terms_returns_empty() {
    let store = setup_memory_store("term_stats_empty");
    store
        .upsert_document(make_document(Uuid::new_v4(), "t", "body"))
        .await
        .unwrap();

    let stats = store
        .term_stats(TextTermStatsRequest {
            terms: vec![],
            filter: Some(ns_filter("test_ns")),
        })
        .await
        .unwrap();
    assert!(stats.is_empty());
}

#[tokio::test]
async fn term_stats_missing_term_has_zero_df() {
    let store = setup_memory_store("term_stats_missing");
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "doc",
            "only this content exists",
        ))
        .await
        .unwrap();

    let stats = store
        .term_stats(TextTermStatsRequest {
            terms: vec!["xyzzy_nonexistent".to_string()],
            filter: Some(ns_filter("test_ns")),
        })
        .await
        .unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].document_frequency, 0);
}
