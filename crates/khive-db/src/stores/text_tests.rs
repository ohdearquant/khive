use super::*;
use crate::pool::PoolConfig;

fn setup_memory_store(table_key: &str) -> Fts5TextSearch {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    {
        let writer = pool.writer().unwrap();
        ensure_fts5_schema(writer.conn(), table_key).unwrap();
    }

    Fts5TextSearch::new(pool, false, table_key.to_string())
}

/// #397 Finding 2 regression fixture: the FTS5 `trigram` tokenizer, matching
/// `StorageBackend::text()`'s production default (`backend.rs`) byte-for-byte
/// (`tokenize = 'trigram'`), rather than the bare `ensure_fts5_schema` helper
/// above, which omits `tokenize=` and so falls back to SQLite's own default
/// (`unicode61`). Production generic search (`operations.rs`, Plain mode) and
/// AnyTerm search both run against trigram-tokenized tables; a regression
/// covered only under the test helper's `unicode61` default would miss
/// trigram-specific behavior entirely.
fn setup_trigram_store(table_key: &str) -> Fts5TextSearch {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    {
        let writer = pool.writer().unwrap();
        let table_name = format!("fts_{}", table_key);
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING fts5(\
             subject_id UNINDEXED, \
             kind UNINDEXED, \
             title, \
             body, \
             tags UNINDEXED, \
             namespace UNINDEXED, \
             metadata UNINDEXED, \
             updated_at UNINDEXED, \
             tokenize = 'trigram'\
             )",
            table_name
        );
        writer.conn().execute_batch(&ddl).unwrap();
    }

    Fts5TextSearch::new(pool, false, table_key.to_string())
}

fn make_document(subject_id: Uuid, title: &str, body: &str) -> TextDocument {
    TextDocument {
        subject_id,
        kind: SubstrateKind::Note,
        title: if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        },
        body: body.to_string(),
        tags: vec![],
        namespace: "test_ns".to_string(),
        metadata: None,
        updated_at: Utc::now(),
    }
}

fn ns_filter(namespace: &str) -> TextFilter {
    TextFilter {
        namespaces: vec![namespace.to_string()],
        ..TextFilter::default()
    }
}

#[tokio::test]
async fn test_upsert_and_search() {
    let store = setup_memory_store("upsert_search");

    let id = Uuid::new_v4();
    let doc = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Entity,
        title: Some("Rust Programming".to_string()),
        body: "Rust is a systems programming language focused on safety and performance."
            .to_string(),
        tags: vec!["rust".to_string(), "programming".to_string()],
        namespace: "tech".to_string(),
        metadata: None,
        updated_at: Utc::now(),
    };

    store.upsert_document(doc).await.unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "Rust programming".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("tech")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id);
    assert_eq!(hits[0].rank, 1);
    assert!(hits[0].score.to_f64() > 0.0);
    assert!(hits[0].title.is_some());
}

#[tokio::test]
async fn test_phrase_search() {
    let store = setup_memory_store("phrase");

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    store
        .upsert_document(make_document(
            id1,
            "Animals",
            "The quick brown fox jumps over the lazy dog.",
        ))
        .await
        .unwrap();

    store
        .upsert_document(make_document(
            id2,
            "Colors",
            "The brown paint was quick to dry, unlike the fox.",
        ))
        .await
        .unwrap();

    let hits = store
        .search(TextSearchRequest {
            query: "quick brown fox".to_string(),
            mode: TextQueryMode::Phrase,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id1);

    let hits = store
        .search(TextSearchRequest {
            query: "quick brown fox".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 2);
}

#[tokio::test]
async fn test_delete_document() {
    let store = setup_memory_store("delete");

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    store
        .upsert_document(make_document(id1, "Doc One", "First document content."))
        .await
        .unwrap();
    store
        .upsert_document(make_document(id2, "Doc Two", "Second document content."))
        .await
        .unwrap();

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.document_count, 2);

    let deleted = store.delete_document("test_ns", id1).await.unwrap();
    assert!(deleted);

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.document_count, 1);

    let deleted_again = store.delete_document("test_ns", id1).await.unwrap();
    assert!(!deleted_again);

    let doc = store.get_document("test_ns", id2).await.unwrap();
    assert!(doc.is_some());

    let doc = store.get_document("test_ns", id1).await.unwrap();
    assert!(doc.is_none());
}

#[tokio::test]
async fn test_count_with_filter() {
    let store = setup_memory_store("count_filter");
    let ns = "test_ns".to_string();

    for i in 0..5 {
        let kind = if i % 2 == 0 {
            SubstrateKind::Entity
        } else {
            SubstrateKind::Note
        };
        let doc = TextDocument {
            subject_id: Uuid::new_v4(),
            kind,
            title: Some(format!("Doc {}", i)),
            body: format!("Content for document number {}", i),
            tags: vec![],
            namespace: ns.clone(),
            metadata: None,
            updated_at: Utc::now(),
        };
        store.upsert_document(doc).await.unwrap();
    }

    let total = store
        .count(TextFilter {
            namespaces: vec![ns.clone()],
            ..TextFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(total, 5);

    let entities = store
        .count(TextFilter {
            namespaces: vec![ns.clone()],
            kinds: vec![SubstrateKind::Entity],
            ..TextFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(entities, 3);

    let notes = store
        .count(TextFilter {
            namespaces: vec![ns.clone()],
            kinds: vec![SubstrateKind::Note],
            ..TextFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(notes, 2);
}

#[tokio::test]
async fn test_get_document_roundtrip() {
    let store = setup_memory_store("get_roundtrip");

    let id = Uuid::new_v4();
    let original = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Note,
        title: Some("Important Memo".to_string()),
        body: "This memo contains critical information.".to_string(),
        tags: vec!["important".to_string(), "memo".to_string()],
        namespace: "work".to_string(),
        metadata: Some(serde_json::json!({"priority": "high"})),
        updated_at: Utc::now(),
    };

    store.upsert_document(original.clone()).await.unwrap();

    let retrieved = store.get_document("work", id).await.unwrap().unwrap();
    assert_eq!(retrieved.subject_id, id);
    assert_eq!(retrieved.kind, SubstrateKind::Note);
    assert_eq!(retrieved.title, Some("Important Memo".to_string()));
    assert_eq!(retrieved.body, "This memo contains critical information.");
    assert_eq!(retrieved.tags, vec!["important", "memo"]);
    assert_eq!(retrieved.namespace, "work");
}

#[tokio::test]
async fn test_upsert_replaces_existing() {
    let store = setup_memory_store("replace");

    let id = Uuid::new_v4();
    store
        .upsert_document(make_document(id, "Original", "Original body text."))
        .await
        .unwrap();

    store
        .upsert_document(make_document(id, "Updated", "Updated body text."))
        .await
        .unwrap();

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.document_count, 1);

    let doc = store.get_document("test_ns", id).await.unwrap().unwrap();
    assert_eq!(doc.title, Some("Updated".to_string()));
    assert_eq!(doc.body, "Updated body text.");
}

#[tokio::test]
async fn test_batch_upsert() {
    let store = setup_memory_store("batch");

    let docs: Vec<TextDocument> = (0..50)
        .map(|i| TextDocument {
            subject_id: Uuid::new_v4(),
            kind: SubstrateKind::Entity,
            title: Some(format!("Item {}", i)),
            body: format!("This is the body content for item number {}", i),
            tags: vec![format!("tag_{}", i % 5)],
            namespace: "batch_ns".to_string(),
            metadata: None,
            updated_at: Utc::now(),
        })
        .collect();

    let summary = store.upsert_documents(docs).await.unwrap();
    assert_eq!(summary.attempted, 50);
    assert_eq!(summary.affected, 50);
    assert_eq!(summary.failed, 0);

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.document_count, 50);
}

#[tokio::test]
async fn test_empty_search() {
    let store = setup_memory_store("empty");

    let hits = store
        .search(TextSearchRequest {
            query: "nonexistent".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(ns_filter("test_ns")),
            top_k: 10,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert!(hits.is_empty());
}

#[tokio::test]
async fn test_rebuild() {
    let store = setup_memory_store("rebuild");

    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "Test",
            "Test document for rebuild.",
        ))
        .await
        .unwrap();

    let stats = store.rebuild(IndexRebuildScope::Full).await.unwrap();
    assert_eq!(stats.document_count, 1);
    assert!(!stats.needs_rebuild);
    assert!(stats.last_rebuild_at.is_some());
}

#[tokio::test]
async fn test_search_with_kind_filter() {
    let store = setup_memory_store("filter_kind");

    let id_entity = Uuid::new_v4();
    let id_note = Uuid::new_v4();

    store
        .upsert_document(TextDocument {
            subject_id: id_entity,
            kind: SubstrateKind::Entity,
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
    // #570: full operator-class matrix
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

/// Round-2 codex re-review regression: `sanitize_fts5_token_group` must keep
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

/// #397 Finding 2 regression: production defaults to the FTS5 `trigram`
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

/// #397 Finding 2 concrete broadening regression: a hyphenated date query
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

/// #397 Finding 2 round-3 regression: a punctuated operand of an FTS5
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

/// #570: all FTS5 operator classes must not crash the generic text search surface.
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
/// either, exercising both the apostrophe (#570) and dollar-sign (#388) fixes together.
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

async fn assert_slash_query_excludes_merged_alias(store: Fts5TextSearch, tokenizer: &str) {
    let slash_id = Uuid::new_v4();
    let merged_distractor_id = Uuid::new_v4();

    store
        .upsert_document(make_document(
            slash_id,
            "slash spelling",
            "the link sustained 900 GB/s transfer rates",
        ))
        .await
        .unwrap();
    store
        .upsert_document(make_document(
            merged_distractor_id,
            "merged spelling",
            "the link sustained 900 GBs transfer rates",
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
                query: "GB/s".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        let hit_ids: std::collections::HashSet<_> = hits.iter().map(|hit| hit.subject_id).collect();

        assert_eq!(
            hit_ids,
            std::collections::HashSet::from([slash_id]),
            "{tokenizer} {mode:?} GB/s query must exclude GBs alias {merged_distractor_id}; got {hit_ids:?}"
        );
    }
}

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

async fn assert_slash_term_stats_exclude_merged_alias(store: Fts5TextSearch, tokenizer: &str) {
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "slash spelling",
            "the link sustained 900 GB/s transfer rates",
        ))
        .await
        .unwrap();
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "merged spelling",
            "the link sustained 900 GBs transfer rates",
        ))
        .await
        .unwrap();

    let stats = store
        .term_stats(TextTermStatsRequest {
            terms: vec!["GB/s".to_string()],
            filter: Some(ns_filter("test_ns")),
        })
        .await
        .unwrap();

    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].sanitized_term, "\"GB/s\"");
    assert_eq!(
        stats[0].document_frequency, 1,
        "{tokenizer} term stats must count only the slash-bearing document"
    );
    assert_eq!(stats[0].document_count, 2);
}

#[tokio::test]
async fn term_stats_slash_literal_excludes_merged_alias_all_tokenizers() {
    assert_slash_term_stats_exclude_merged_alias(
        setup_memory_store("unicode61_term_stats_slash_alias"),
        "unicode61",
    )
    .await;
    assert_slash_term_stats_exclude_merged_alias(
        setup_trigram_store("trigram_term_stats_slash_alias"),
        "trigram",
    )
    .await;
}

/// Dropping the FTS5 virtual table makes every per-item INSERT in the batch
/// fail with a SQLite error.  Each failure is caught by the SAVEPOINT so the
/// outer transaction still commits and the method returns Ok.
///
/// Regression: before the fix, `first_error` was always `String::new()` even
/// when `failed > 0`.  This test is RED against the unfixed code and GREEN
/// after the fix.
#[tokio::test]
async fn upsert_documents_first_error_populated_on_item_failure() {
    let table_key = "first_err_fts";

    // Keep a clone of the pool so we can manipulate the schema before the batch.
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        ensure_fts5_schema(writer.conn(), table_key).unwrap();
    }
    let store = Fts5TextSearch::new(Arc::clone(&pool), false, table_key.to_string());

    // Drop the FTS5 virtual table (which also removes all its shadow tables).
    // Every subsequent DELETE/INSERT on the table will fail with "no such table".
    // Each failure is isolated by a SAVEPOINT, so the outer transaction commits.
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(&format!("DROP TABLE fts_{}", table_key))
            .expect("drop FTS5 virtual table");
    }

    let docs = vec![
        make_document(Uuid::new_v4(), "Doc A", "body a"),
        make_document(Uuid::new_v4(), "Doc B", "body b"),
    ];

    let summary = store.upsert_documents(docs).await.unwrap();

    assert!(
        summary.failed > 0,
        "expected at least one item to fail after the FTS5 table was dropped"
    );
    assert!(
        !summary.first_error.is_empty(),
        "first_error must describe the failure when failed > 0, \
         but got an empty string; the error is being silently swallowed"
    );
}

/// ADR-067 Component A entry 4: with `KHIVE_WRITE_QUEUE=1`, `upsert_documents`
/// routes through the WriterTask channel instead of the pool-mutex path, and
/// both documents are actually committed and independently readable back.
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: true`), not
/// the `KHIVE_WRITE_QUEUE` env var — that env var is process-global and this
/// crate's other tests are NOT `#[serial]` against it, so a window where it
/// is set here could leak into a concurrently-scheduled test's own pool
/// construction (ADR-067 Fork C slice 2 round 2, LOW finding).
#[tokio::test]
async fn upsert_documents_routes_through_writer_task_when_flag_enabled() {
    let table_key = "write_queue_flag_test";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_text.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        ensure_fts5_schema(writer.conn(), table_key).unwrap();
    }

    let store = Fts5TextSearch::new(Arc::clone(&pool), true, table_key.to_string());

    let subject1 = Uuid::new_v4();
    let subject2 = Uuid::new_v4();
    let docs = vec![
        make_document(subject1, "Doc A", "body a"),
        make_document(subject2, "Doc B", "body b"),
    ];

    let summary = store.upsert_documents(docs).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    assert!(store
        .get_document("test_ns", subject1)
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_document("test_ns", subject2)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}
