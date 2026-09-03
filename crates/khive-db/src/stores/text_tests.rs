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

/// #397 regression fixture: the FTS5 `trigram` tokenizer, matching
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
             record_kind, \
             tokenize = 'trigram'\
             )",
            table_name
        );
        writer.conn().execute_batch(&ddl).unwrap();
        writer
            .conn()
            .execute_batch(&rowid_map_ddl(&table_name))
            .unwrap();
    }

    Fts5TextSearch::new(pool, false, table_key.to_string())
}

fn make_document(subject_id: Uuid, title: &str, body: &str) -> TextDocument {
    TextDocument {
        subject_id,
        kind: SubstrateKind::Note,
        record_kind: None,
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

fn make_record_kind_document(
    subject_id: Uuid,
    record_kind: &str,
    title: &str,
    body: &str,
) -> TextDocument {
    TextDocument {
        record_kind: Some(record_kind.to_string()),
        ..make_document(subject_id, title, body)
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
        record_kind: None,
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

/// Round 2 review finding 3: a stale map entry — left behind by a crash
/// between an FTS-row delete and its own map-row delete, before this fix —
/// must not make `get_document`/`delete_document` operate on whatever
/// unrelated document FTS5 later reuses that freed rowid for. Seeds A,
/// removes A's FTS row directly via raw SQL while leaving its map entry
/// dangling (reproducing the crash window), inserts B at the exact freed
/// rowid (pinned explicitly rather than relying on FTS5's own rowid-reuse
/// policy), then asserts `get_document(A)` is `None`, `delete_document(A)`
/// returns `false`, and B is untouched.
#[tokio::test]
async fn stale_map_entry_never_returns_or_deletes_the_wrong_document() {
    let store = setup_memory_store("stale_map_wrong_doc");
    let ns = "test_ns";

    let a = Uuid::new_v4();
    store
        .upsert_document(make_document(a, "Doc A", "first document"))
        .await
        .unwrap();

    let table = store.table_name.clone();
    let a_rowid: i64 = {
        let writer = store.pool.writer().unwrap();
        let rowid: i64 = writer
            .conn()
            .query_row(
                &format!("SELECT rowid FROM {table} WHERE subject_id = ?1"),
                rusqlite::params![a.to_string()],
                |row| row.get(0),
            )
            .expect("read A's rowid");
        writer
            .conn()
            .execute(
                &format!("DELETE FROM {table} WHERE rowid = ?1"),
                rusqlite::params![rowid],
            )
            .expect("delete A's FTS row directly, leaving its map entry stale");
        rowid
    };

    let b = Uuid::new_v4();
    {
        let writer = store.pool.writer().unwrap();
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                     (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                      updated_at, record_kind) \
                     VALUES (?1, ?2, 'note', '', 'second document', '[]', ?3, NULL, 0, NULL)"
                ),
                rusqlite::params![a_rowid, b.to_string(), ns],
            )
            .expect("insert B reusing A's freed rowid");
        // A real upsert would also write B's own map entry — do that here so
        // the assertions below isolate A's stale entry as the only anomaly.
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table}_rowids (namespace, subject_id, rowid) VALUES (?1, ?2, ?3)"
                ),
                rusqlite::params![ns, b.to_string(), a_rowid],
            )
            .expect("insert B's own map entry");
    }

    let fetched_a = store.get_document(ns, a).await.unwrap();
    assert!(
        fetched_a.is_none(),
        "a stale map entry must not resolve get_document(A) to B's document, got {fetched_a:?}"
    );

    let deleted_a = store.delete_document(ns, a).await.unwrap();
    assert!(
        !deleted_a,
        "a stale map entry must not let delete_document(A) delete B's row"
    );

    let fetched_b = store.get_document(ns, b).await.unwrap();
    assert!(
        fetched_b.is_some(),
        "B's document must remain intact after A's stale-map operations"
    );
}

/// Round 3 review: `delete_document_dml`'s unmanaged (no-writer-task) path
/// wraps the FTS-row delete and the map-row delete in one explicit `BEGIN
/// IMMEDIATE` / `COMMIT` / `ROLLBACK` block (`Fts5TextSearch::delete_document`'s
/// fallback, used when `current_writer_task` returns `None`) so a failure
/// between the two statements rolls both back together instead of leaving the
/// FTS row deleted while its map row survives. Forces that failure via the
/// `delete_dml_test_seam` flag and asserts on the raw rows that neither
/// statement's effect was left half-committed, then clears the seam and
/// confirms a real delete still removes both rows.
///
/// `#[serial_test::serial]`: the seam flag is a process-wide `AtomicBool`
/// (see `delete_dml_test_seam`'s doc comment for why), so this test cannot
/// run concurrently with anything else that calls `delete_document` on the
/// unmanaged path.
#[tokio::test]
#[serial_test::serial(delete_dml_test_seam)]
async fn unmanaged_delete_rolls_back_the_fts_row_when_the_map_delete_fails() {
    let store = setup_memory_store("unmanaged_delete_rollback");
    assert!(
        store
            .current_writer_task("precondition_check")
            .unwrap()
            .is_none(),
        "this test exercises the no-writer-task fallback path; a writer-task \
         handle here would route delete_document through the queue's own \
         transaction instead of the explicit BEGIN IMMEDIATE block under test"
    );

    let ns = "test_ns";
    let a = Uuid::new_v4();
    store
        .upsert_document(make_document(a, "Doc A", "first document"))
        .await
        .unwrap();

    let table = store.table_name.clone();
    let map_table = rowid_map_table(&table);

    struct ResetSeamOnDrop;
    impl Drop for ResetSeamOnDrop {
        fn drop(&mut self) {
            delete_dml_test_seam::FAIL_BEFORE_MAP_DELETE
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let reset_seam = ResetSeamOnDrop;
    delete_dml_test_seam::FAIL_BEFORE_MAP_DELETE.store(true, std::sync::atomic::Ordering::SeqCst);

    let result = store.delete_document(ns, a).await;
    assert!(
        result.is_err(),
        "the forced map-row-delete failure must surface as an error, got {result:?}"
    );

    {
        let writer = store.pool.writer().unwrap();
        let fts_count: i64 = writer
            .conn()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE subject_id = ?1"),
                rusqlite::params![a.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_count, 1,
            "the FTS-row delete must roll back alongside the failed map-row delete"
        );

        let map_count: i64 = writer
            .conn()
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {map_table} WHERE namespace = ?1 AND subject_id = ?2"
                ),
                rusqlite::params![ns, a.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            map_count, 1,
            "the map row must survive a rolled-back delete alongside its FTS row"
        );
    }

    let fetched = store.get_document(ns, a).await.unwrap();
    assert!(
        fetched.is_some(),
        "a rolled-back delete must leave the document fully readable, got {fetched:?}"
    );

    drop(reset_seam);

    let deleted = store.delete_document(ns, a).await.unwrap();
    assert!(
        deleted,
        "with the seam cleared, delete_document must succeed and remove both rows"
    );

    let writer = store.pool.writer().unwrap();
    let fts_count: i64 = writer
        .conn()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE subject_id = ?1"),
            rusqlite::params![a.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        fts_count, 0,
        "the FTS row must be gone after the real delete"
    );

    let map_count: i64 = writer
        .conn()
        .query_row(
            &format!("SELECT COUNT(*) FROM {map_table} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![ns, a.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        map_count, 0,
        "the map row must be gone after the real delete"
    );
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

/// The pre-fix `DELETE` scanned the whole FTS5 virtual
/// table because `namespace`/`subject_id` are `UNINDEXED` columns; the
/// rowid-map version resolves the same lookup through the map's primary key
/// instead. This test pins both plan shapes as strings measured against a
/// live `EXPLAIN QUERY PLAN`, not shapes it merely expects, and keeps the OLD
/// statement inline as the control so a future edit to either builder cannot
/// silently make this comparison meaningless.
#[test]
fn delete_statement_plan_uses_the_map_not_a_table_scan() {
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
    ensure_fts5_schema(&conn, "plan_check").expect("schema");
    let table = "fts_plan_check";

    // The exact statement `delete_document_statement` issued before this fix.
    // `VIRTUAL TABLE INDEX 0:` with NO trailing operator is FTS5's plan label
    // for an unconstrained cursor: every row is opened and tested, because
    // `namespace`/`subject_id` are UNINDEXED and xBestIndex has nothing to
    // push down. That bare, operator-less form is the control this test
    // pins — measured live, not assumed.
    const OLD_DELETE_SQL: &str = "DELETE FROM {table} WHERE namespace = ?1 AND subject_id = ?2";
    let old_sql = OLD_DELETE_SQL.replace("{table}", table);
    let old_plan = explain_query_plan(
        &conn,
        &old_sql,
        &["local", "00000000-0000-0000-0000-000000000000"],
    );
    let unconstrained_scan = format!("SCAN {table} VIRTUAL TABLE INDEX 0:");
    assert!(
        old_plan
            .iter()
            .any(|step| step.trim_end() == unconstrained_scan.trim_end()),
        "control (pre-fix) plan must be an unconstrained FTS5 scan: {old_plan:?}"
    );

    let new_statement = delete_document_statement(
        table,
        "local",
        Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap(),
    );
    let new_plan = explain_query_plan(
        &conn,
        &new_statement.sql,
        &["local", "00000000-0000-0000-0000-000000000000"],
    );
    assert!(
        !new_plan.iter().any(|step| step.trim_end() == unconstrained_scan.trim_end()),
        "fixed plan must not fall back to the unconstrained FTS5 scan the control used: {new_plan:?}"
    );
    assert!(
        new_plan
            .iter()
            .any(|step| step.contains("SEARCH fts_plan_check_rowids USING PRIMARY KEY")),
        "fixed plan must search the rowid map by its primary key: {new_plan:?}"
    );
    assert!(
        new_plan
            .iter()
            .any(|step| step.contains(&format!("SCAN {table} VIRTUAL TABLE INDEX 0:="))),
        "fixed plan's FTS5 access must be constrained (rowid equality driven by the map's \
         primary-key search), not the bare unconstrained cursor: {new_plan:?}"
    );
}

/// Runs `EXPLAIN QUERY PLAN` for `sql` bound with `params` (all bound as
/// text — sufficient for this file's plan-shape assertions) and returns each
/// step's human-readable `detail` column.
fn explain_query_plan(conn: &rusqlite::Connection, sql: &str, params: &[&str]) -> Vec<String> {
    let plan_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stmt = conn.prepare(&plan_sql).expect("prepare EXPLAIN QUERY PLAN");
    let params: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect();
    let rows = stmt
        .query_map(params.as_slice(), |row| row.get::<_, String>(3))
        .expect("query plan rows");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect plan steps")
}

/// `insert_document_map_statement`'s `last_insert_rowid()` must
/// resolve to the FTS5 `INSERT` that ran immediately before it on the same
/// connection — this is the whole correctness argument for keeping the two
/// statements adjacent instead of assuming SQLite's `INSERT`-then-read-back
/// contract without checking it against the actual FTS5 virtual table
/// implementation.
#[test]
fn last_insert_rowid_reflects_the_fts5_insert() {
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
    ensure_fts5_schema(&conn, "rowid_check").expect("schema");
    let table = "fts_rowid_check";

    let doc = make_document(Uuid::new_v4(), "Title", "Body");
    let insert_stmt = insert_document_statement(table, &doc);
    {
        let mut stmt = conn.prepare(&insert_stmt.sql).expect("prepare fts insert");
        crate::sql_bridge::bind_params(&mut stmt, &insert_stmt.params).expect("bind fts insert");
        stmt.raw_execute().expect("insert fts row");
    }
    let observed_rowid = conn.last_insert_rowid();

    let actual_rowid: i64 = conn
        .query_row(
            &format!("SELECT rowid FROM {table} WHERE subject_id = ?1"),
            rusqlite::params![doc.subject_id.to_string()],
            |row| row.get(0),
        )
        .expect("read back inserted row's rowid");
    assert_eq!(
        observed_rowid, actual_rowid,
        "last_insert_rowid() must equal the FTS5 insert's own rowid"
    );
}

/// Not a CI test — a manual scale measurement, run with:
/// `cargo test -p khive-db --lib scale_measurement_delete_by_subject -- --ignored --nocapture`
///
/// Builds a 200,000-row, ~1.6 KB-body `fts_notes`-shaped table on disk under
/// `/private/tmp/` (production-shaped: file-backed, trigram-tokenized,
/// non-trivial body text — an in-memory table or tiny bodies would not
/// reproduce the overflow-page I/O the pre-fix scan pays for), then times
/// 100 deletes by subject through the OLD (pre-fix, literal) statement and
/// 100 through the NEW (rowid-map) statement, against the same corpus.
#[test]
#[ignore = "manual scale measurement, not a CI assertion — see doc comment"]
fn scale_measurement_delete_by_subject() {
    let path = std::path::PathBuf::from(format!(
        "/private/tmp/khive-fts-scale-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).expect("open scratch db under /private/tmp");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE fts_notes USING fts5(\
         subject_id UNINDEXED, kind UNINDEXED, title, body, tags UNINDEXED, \
         namespace UNINDEXED, metadata UNINDEXED, updated_at UNINDEXED, record_kind, \
         tokenize = 'trigram')",
    )
    .expect("create production-shaped fts_notes");
    conn.execute_batch(&rowid_map_ddl("fts_notes"))
        .expect("create rowid map");

    const ROW_COUNT: usize = 200_000;
    const DELETE_COUNT: usize = 100;
    let body_filler = "the quick brown fox jumps over the lazy dog ".repeat(35); // ~1.6 KB
    let ids: Vec<Uuid> = (0..ROW_COUNT).map(|_| Uuid::new_v4()).collect();

    let seed_start = std::time::Instant::now();
    conn.execute_batch("BEGIN").expect("begin seed");
    {
        let mut insert = conn
            .prepare(
                "INSERT INTO fts_notes \
                 (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, 'note', '', ?2, '[]', 'local', NULL, 0, 'memory')",
            )
            .expect("prepare seed insert");
        for id in &ids {
            let body = format!("{body_filler}{id}");
            insert
                .execute(rusqlite::params![id.to_string(), body])
                .expect("insert seed row");
        }
    }
    conn.execute_batch("COMMIT").expect("commit seed");
    conn.execute_batch(
        "INSERT OR REPLACE INTO fts_notes_rowids (namespace, subject_id, rowid) \
         SELECT namespace, subject_id, rowid FROM fts_notes ORDER BY rowid ASC",
    )
    .expect("backfill rowid map");
    eprintln!(
        "seeded {ROW_COUNT} rows (~{} bytes/body) in {:?}",
        body_filler.len(),
        seed_start.elapsed()
    );

    // OLD: the literal statement `delete_document_statement` issued before
    // this fix — namespace/subject_id are UNINDEXED, so this is a full scan.
    const OLD_DELETE_SQL: &str = "DELETE FROM fts_notes WHERE namespace = ?1 AND subject_id = ?2";
    let old_start = std::time::Instant::now();
    {
        let mut stmt = conn.prepare(OLD_DELETE_SQL).expect("prepare old delete");
        for id in &ids[0..DELETE_COUNT] {
            let affected = stmt
                .execute(rusqlite::params!["local", id.to_string()])
                .expect("old delete");
            assert_eq!(affected, 1);
        }
    }
    let old_elapsed = old_start.elapsed();

    // NEW: the rowid-map statement, against the (now 200,000 - 100 row, i.e.
    // still effectively 200,000-row) same corpus.
    let new_statement_sql = delete_document_statement("fts_notes", "local", ids[DELETE_COUNT]).sql;
    let new_start = std::time::Instant::now();
    {
        let mut fts_stmt = conn
            .prepare(&new_statement_sql)
            .expect("prepare new delete");
        let mut map_stmt = conn
            .prepare("DELETE FROM fts_notes_rowids WHERE namespace = ?1 AND subject_id = ?2")
            .expect("prepare new delete map");
        for id in &ids[DELETE_COUNT..DELETE_COUNT * 2] {
            let affected = fts_stmt
                .execute(rusqlite::params!["local", id.to_string()])
                .expect("new delete");
            assert_eq!(affected, 1);
            map_stmt
                .execute(rusqlite::params!["local", id.to_string()])
                .expect("new delete map row");
        }
    }
    let new_elapsed = new_start.elapsed();

    let ratio = old_elapsed.as_secs_f64() / new_elapsed.as_secs_f64().max(1e-9);
    eprintln!(
        "{DELETE_COUNT} deletes by subject over {ROW_COUNT} rows: \
         OLD (full scan) = {old_elapsed:?}, NEW (rowid map) = {new_elapsed:?}, ratio = {ratio:.1}x"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));

    assert!(
        new_elapsed < old_elapsed,
        "the rowid-map delete must be faster than the full-scan delete at this scale \
         (old={old_elapsed:?}, new={new_elapsed:?})"
    );
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
            record_kind: None,
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
        record_kind: Some("memory".to_string()),
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
    assert_eq!(retrieved.record_kind.as_deref(), Some("memory"));
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
            record_kind: None,
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
        record_kind: None,
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
        record_kind: None,
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
        record_kind: None,
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
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: Some(true)`), not
/// the `KHIVE_WRITE_QUEUE` env var — that env var is process-global and this
/// crate's other tests are NOT `#[serial]` against it, so a window where it
/// is set here could leak into a concurrently-scheduled test's own pool
/// construction (ADR-067 Component A).
#[tokio::test]
async fn upsert_documents_routes_through_writer_task_when_flag_enabled() {
    let table_key = "write_queue_flag_test";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_text.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
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

/// #1064 regression: a hyphenated identifier token mixed with plain keyword
/// terms (`"ADR-086 workspace mirror"`) must still surface the document whose
/// title/body contains that identifier under the production `trigram`
/// tokenizer, in both `Plain` and `AnyTerm` mode.
#[tokio::test]
async fn test_search_hyphenated_id_with_plain_terms_matches_exact_id() {
    let store = setup_trigram_store("issue_1064_hyphenated_multiword");
    let target_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            target_id,
            "ADR-086",
            "ADR-086 workspace mirror amendment document",
        ))
        .await
        .unwrap();
    let unrelated_id = Uuid::new_v4();
    store
        .upsert_document(make_document(
            unrelated_id,
            "unrelated",
            "completely unrelated gardening content",
        ))
        .await
        .unwrap();

    for mode in [TextQueryMode::Plain, TextQueryMode::AnyTerm] {
        let hits = store
            .search(TextSearchRequest {
                query: "ADR-086 workspace mirror".to_string(),
                mode: mode.clone(),
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
            .unwrap();
        let hit_ids: std::collections::HashSet<_> = hits.iter().map(|h| h.subject_id).collect();
        assert!(
            hit_ids.contains(&target_id),
            "#1064 {mode:?} query must match {target_id}, got {hit_ids:?}"
        );
        assert!(
            !hit_ids.contains(&unrelated_id),
            "#1064 {mode:?} query must not match unrelated doc, got {hit_ids:?}"
        );
    }
}

// `fts_passes` is an issued counter (khive_storage::usage): it must count
// one FTS5 statement actually prepared and executed. An empty/fully-sanitized
// query short-circuits `search()` before any statement exists (`build_match_expr`
// returns `None`) and must not count; a normal query that does reach
// `conn.prepare` must count exactly once.
#[tokio::test]
async fn test_search_empty_query_does_not_count_fts_pass() {
    let store = setup_memory_store("empty_query_no_count");

    let ctx = khive_storage::usage::UsageContext::new();
    let hits = khive_storage::usage::scope(ctx.clone(), async {
        store
            .search(TextSearchRequest {
                query: String::new(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
    })
    .await
    .unwrap();

    assert!(hits.is_empty(), "empty query must return no hits");
    let snap = ctx.snapshot();
    assert!(
        snap.get("fts_passes").is_none(),
        "an empty query must never prepare an FTS5 statement, so fts_passes \
         must not count; got {snap:?}"
    );
}

#[tokio::test]
async fn test_search_fully_sanitized_query_does_not_count_fts_pass() {
    let store = setup_memory_store("sanitized_query_no_count");

    // Pure FTS5 metacharacters — `sanitize_fts5_token_group` strips every
    // token to nothing, so `build_match_expr` still returns `None`.
    let ctx = khive_storage::usage::UsageContext::new();
    let hits = khive_storage::usage::scope(ctx.clone(), async {
        store
            .search(TextSearchRequest {
                query: "\"\"^^**".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
    })
    .await
    .unwrap();

    assert!(hits.is_empty(), "fully-sanitized query must return no hits");
    let snap = ctx.snapshot();
    assert!(
        snap.get("fts_passes").is_none(),
        "a fully-sanitized query must never prepare an FTS5 statement, so \
         fts_passes must not count; got {snap:?}"
    );
}

#[tokio::test]
async fn test_search_normal_query_counts_exactly_one_fts_pass() {
    let store = setup_memory_store("normal_query_counts_once");

    let id = Uuid::new_v4();
    store
        .upsert_document(make_document(id, "counted", "fts pass accounting body"))
        .await
        .unwrap();

    let ctx = khive_storage::usage::UsageContext::new();
    let hits = khive_storage::usage::scope(ctx.clone(), async {
        store
            .search(TextSearchRequest {
                query: "accounting".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 64,
            })
            .await
    })
    .await
    .unwrap();

    assert_eq!(hits.len(), 1, "query must match the upserted document");
    let snap = ctx.snapshot();
    assert_eq!(
        snap["fts_passes"], 1,
        "a query that reaches conn.prepare must count exactly one fts_pass; got {snap:?}"
    );
}

#[tokio::test]
async fn test_cancelled_search_does_not_issue_fts_pass() {
    let store = Arc::new(setup_memory_store("cancelled_search_counts"));
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "counted",
            "cancelled continuation accounting",
        ))
        .await
        .unwrap();

    let pool = Arc::clone(&store.pool);
    let reader_blocker = pool.writer().unwrap();
    let ctx = khive_storage::usage::UsageContext::new();
    let blocked_store = Arc::clone(&store);
    let task = tokio::spawn(khive_storage::usage::scope(ctx.clone(), async move {
        blocked_store
            .search(TextSearchRequest {
                query: "continuation".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 0,
            })
            .await
    }));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    task.abort();
    let joined = task.await;
    assert!(joined.unwrap_err().is_cancelled());
    assert!(
        ctx.snapshot().get("fts_passes").is_none(),
        "a search cancelled during reader checkout must not issue an FTS pass"
    );
    drop(reader_blocker);

    let hits = khive_storage::usage::scope(ctx.clone(), async {
        store
            .search(TextSearchRequest {
                query: "continuation".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(ns_filter("test_ns")),
                top_k: 10,
                snippet_chars: 0,
            })
            .await
    })
    .await
    .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the pool must remain usable after cancellation"
    );
    assert_eq!(
        ctx.snapshot()["fts_passes"],
        1,
        "only the follow-up search may issue an FTS pass after cancellation"
    );
}

#[tokio::test]
async fn test_search_unranked_counts_exactly_one_fts_pass() {
    let store = setup_memory_store("unranked_counts_once");
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "counted",
            "unranked pass accounting",
        ))
        .await
        .unwrap();

    let ctx = khive_storage::usage::UsageContext::new();
    khive_storage::usage::scope(ctx.clone(), async {
        store
            .search_with_options(
                TextSearchRequest {
                    query: "unranked".to_string(),
                    mode: TextQueryMode::Plain,
                    filter: Some(ns_filter("test_ns")),
                    top_k: 10,
                    snippet_chars: 0,
                },
                TextSearchOptions {
                    gather_mode: TextGatherMode::Unranked,
                    gather_limit: None,
                },
            )
            .await
    })
    .await
    .unwrap();

    assert_eq!(ctx.snapshot()["fts_passes"], 1);
}

#[tokio::test]
async fn test_search_rank_within_cap_counts_two_fts_passes() {
    let store = setup_memory_store("rank_within_cap_counts_twice");
    store
        .upsert_document(make_document(
            Uuid::new_v4(),
            "counted",
            "rank within cap accounting",
        ))
        .await
        .unwrap();

    let ctx = khive_storage::usage::UsageContext::new();
    khive_storage::usage::scope(ctx.clone(), async {
        store
            .search_with_options(
                TextSearchRequest {
                    query: "accounting".to_string(),
                    mode: TextQueryMode::Plain,
                    filter: Some(ns_filter("test_ns")),
                    top_k: 10,
                    snippet_chars: 0,
                },
                TextSearchOptions {
                    gather_mode: TextGatherMode::RankWithinCap,
                    gather_limit: Some(20),
                },
            )
            .await
    })
    .await
    .unwrap();

    assert_eq!(ctx.snapshot()["fts_passes"], 2);
}

/// ADR-136 D1 gate 2/4: `rename_namespace`'s flag-on path must route through
/// the pool-wide `WriterTask`, not `with_writer_unmanaged`'s
/// standalone-connection path, when the write queue is enabled — same
/// occupier / `queue_depth()` technique as
/// `upsert_documents_routes_through_writer_task_when_flag_enabled` above (a
/// `writer_task_spawn_count() == 1` assertion alone is a false positive:
/// `upsert_document` setup calls already spawn/use the task). Red-proof:
/// reverting the `current_writer_task("fts_rename_namespace")` branch in
/// `rename_namespace` (forcing every call through `with_writer_unmanaged`)
/// makes `saw_enqueued` stay `false` and this test fail — see the impl
/// report for the exact revert/run/restore transcript.
#[tokio::test]
async fn rename_namespace_routes_through_writer_task_when_flag_enabled() {
    let table_key = "write_queue_rename_namespace";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_rename_namespace.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        ensure_fts5_schema(writer.conn(), table_key).unwrap();
    }

    let store = Fts5TextSearch::new(Arc::clone(&pool), true, table_key.to_string());

    let subject = Uuid::new_v4();
    let mut doc = make_document(subject, "Doc A", "body a");
    doc.namespace = "old_ns".to_string();
    store.upsert_document(doc).await.unwrap();

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned for a file-backed pool with the flag on");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let rename_task = tokio::spawn(async move { store.rename_namespace("old_ns", "new_ns").await });

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "rename_namespace's write request never appeared in the writer task's channel \
         while the occupier held the single drain slot — rename_namespace is not routing \
         through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");
    let moved = rename_task
        .await
        .expect("rename task must not panic")
        .expect("rename_namespace must succeed once unblocked");
    assert_eq!(moved, 1);
}

/// ADR-136 D1 gate 3/4: with `KHIVE_WRITE_ROUTING=strict` and no writer task
/// available, `rename_namespace` must error instead of silently falling back
/// to `with_writer_unmanaged`'s standalone-connection path.
#[tokio::test]
async fn rename_namespace_strict_routing_fails_closed_without_writer_task() {
    let table_key = "strict_rename_namespace";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict_rename_namespace.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        write_routing_strict: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        ensure_fts5_schema(writer.conn(), table_key).unwrap();
    }

    let store = Fts5TextSearch::new(Arc::clone(&pool), true, table_key.to_string());
    let err = store.rename_namespace("old_ns", "new_ns").await.expect_err(
        "KHIVE_WRITE_ROUTING=strict with no writer task must fail closed, not silently \
             fall back to with_writer_unmanaged",
    );
    assert!(
        err.to_string().contains("strict"),
        "error must name strict routing, got: {err}"
    );
}

/// ADR-136 D1 gate 3 amendment: a store built on a thread with no ambient
/// Tokio runtime caches `writer_task: None` at construction — the pool
/// returns `Err(WriterTaskNoRuntime)`, which `Fts5TextSearch::new` collapses
/// via `.ok().flatten()` (a documented, deliberate best-effort degrade). The
/// bug this guards against: without `with_writer`'s write-time re-lookup
/// (`current_writer_task`), that construction-time `None` would stick
/// forever, so a *normal* FTS write (`upsert_document`, routed through the
/// general `with_writer` helper, not a maintenance path) issued later inside
/// a real runtime would silently bypass the queue via the direct-connection
/// path instead of routing through the shared `WriterTask` like every other
/// write on this pool. Same occupier / `queue_depth()` discriminator as
/// `rename_namespace_routes_through_writer_task_when_flag_enabled` above,
/// proving genuine queue routing rather than a `writer_task_spawn_count() ==
/// 1` false positive.
///
/// Deliberately `#[test]`, not `#[tokio::test]`: construction must happen
/// with no ambient runtime, which a `#[tokio::test]` function body would
/// not give it (the whole test body already runs on a Tokio worker thread).
/// Red-proof: reverting `with_writer`'s `self.current_writer_task(op)` check
/// back to `&self.writer_task` makes `saw_enqueued` stay `false` and this
/// test fail — the write takes the direct-connection path immediately
/// instead of ever appearing in the writer task's channel.
#[test]
fn general_write_routes_through_writer_task_when_store_built_outside_runtime() {
    let table_key = "general_write_no_runtime_construction";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("general_write_no_runtime_construction.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        ensure_fts5_schema(writer.conn(), table_key).unwrap();
    }

    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "sanity: this test body must not already be running inside a Tokio runtime"
    );
    // Construction happens here, outside any runtime — reproduces the
    // permanent-`None`-cache scenario `writer_task_handle()`'s doc comment
    // describes.
    let store = Fts5TextSearch::new(Arc::clone(&pool), true, table_key.to_string());

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        let writer_task = pool
            .writer_task_handle()
            .unwrap()
            .expect("writer task must be available now that a runtime exists");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), StorageError>(())
                    })
                    .await
            })
        };
        started_rx
            .await
            .expect("occupier must signal it has started running inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "channel must start empty once the occupier has been dequeued and is running"
        );

        let write_task = tokio::spawn(async move {
            store
                .upsert_document(make_document(Uuid::new_v4(), "outside-runtime", "body"))
                .await
        });

        let mut saw_enqueued = false;
        for _ in 0..100 {
            if writer_task.queue_depth() >= 1 {
                saw_enqueued = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            saw_enqueued,
            "upsert_document's write request never appeared in the writer task's channel \
             while the occupier held the single drain slot — a store built outside a \
             runtime is not re-checking writer-task availability at write time"
        );

        release_tx
            .send(())
            .expect("occupier must still be waiting on the release signal");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");
        write_task
            .await
            .expect("write task must not panic")
            .expect("upsert_document must succeed once unblocked");
    });
}

/// #1907: memory recall's lexical work must scale with the memory corpus, not
/// with unrelated notes that happen to match the same common query term.
#[test]
fn memory_kind_match_work_is_bounded_by_memory_corpus() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rusqlite::functions::FunctionFlags;

    fn measure(noise_rows: usize) -> (usize, usize, String) {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE fts_notes USING fts5(\
             subject_id UNINDEXED, \
             kind UNINDEXED, \
             title, \
             body, \
             tags UNINDEXED, \
             namespace UNINDEXED, \
             metadata UNINDEXED, \
             updated_at UNINDEXED, \
             record_kind, \
             tokenize = 'trigram'\
             )",
        )
        .expect("production-shaped trigram table");
        conn.execute_batch(&rowid_map_ddl("fts_notes"))
            .expect("production-shaped rowid map");

        conn.execute_batch("BEGIN").expect("begin seed");
        {
            let mut insert = conn
                .prepare(
                    "INSERT INTO fts_notes \
                    (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                     VALUES (?1, 'note', '', 'common recall token', '[]', 'local', NULL, 0, ?2)",
                )
                .expect("prepare seed insert");
            for i in 0..noise_rows {
                insert
                    .execute(rusqlite::params![format!("noise-{i}"), "message"])
                    .expect("insert non-memory note");
            }
            for i in 0..50 {
                insert
                    .execute(rusqlite::params![format!("memory-{i}"), "memory"])
                    .expect("insert memory note");
            }
        }
        conn.execute_batch("COMMIT").expect("commit seed");

        let visits = std::sync::Arc::new(AtomicUsize::new(0));
        let visits_for_fn = std::sync::Arc::clone(&visits);
        conn.create_scalar_function(
            "visit_memory_kind",
            1,
            FunctionFlags::SQLITE_UTF8,
            move |ctx| {
                visits_for_fn.fetch_add(1, Ordering::Relaxed);
                Ok(i64::from(ctx.get::<String>(0)? == "memory"))
            },
        )
        .expect("register visit counter");

        let filter = TextFilter {
            record_kinds: vec!["memory".to_string()],
            ..TextFilter::default()
        };
        let match_expr = build_filtered_match_expr("common", TextQueryMode::Plain, Some(&filter))
            .expect("filtered MATCH expression");
        assert!(
            match_expr.contains("record_kind : \"memory\""),
            "granular kind must be part of MATCH, got {match_expr:?}"
        );
        let sql = format!(
            "SELECT subject_id FROM fts_notes \
             WHERE fts_notes MATCH ?1 \
             AND rank MATCH '{LEXICAL_BM25_RANK}' \
             AND visit_memory_kind(record_kind) = 1 \
             ORDER BY rank LIMIT 20"
        );
        let plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare explain")
            .query_map([&match_expr], |row| row.get::<_, String>(3))
            .expect("query explain")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect explain")
            .join("\n");
        let hits = conn
            .prepare(&sql)
            .expect("prepare measured query")
            .query_map([&match_expr], |row| row.get::<_, String>(0))
            .expect("run measured query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect measured hits");

        (hits.len(), visits.load(Ordering::Relaxed), plan)
    }

    let (small_hits, small_visits, small_plan) = measure(200);
    let (large_hits, large_visits, large_plan) = measure(2_000);
    assert_eq!(small_hits, 20);
    assert_eq!(large_hits, 20);
    assert_eq!(
        small_visits, 20,
        "only the requested memory top-K is visited"
    );
    assert_eq!(large_visits, 20, "unrelated notes must add no visits");
    assert!(small_plan.contains("VIRTUAL TABLE"), "{small_plan}");
    assert!(large_plan.contains("VIRTUAL TABLE"), "{large_plan}");
    assert!(
        large_visits < small_visits.saturating_mul(3),
        "memory query scanned the unrelated note corpus: {small_visits} candidate visits \
         with 200 noise rows versus {large_visits} with 2,000 noise rows\n\
         small plan:\n{small_plan}\nlarge plan:\n{large_plan}"
    );
}
