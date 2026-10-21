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

include!("text_tests/basic_search_tests.rs");

/// A stale map entry — left behind by a crash between an FTS-row delete and
/// its own map-row delete — must not make `get_document`/`delete_document`
/// operate on whatever
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

/// The batch upsert path (`batch_upsert_documents_dml`) must recheck the
/// same key predicate the single-row delete already does: a stale map entry
/// for A pointing at a rowid B now occupies must not let `upsert_documents`
/// delete B while re-inserting A. Reproduces the same crash-window setup as
/// `stale_map_entry_never_returns_or_deletes_the_wrong_document` above, then
/// drives the fix via the batch path instead of the single-document path.
#[tokio::test]
async fn batch_upsert_never_deletes_the_wrong_document_via_a_stale_map_entry() {
    let store = setup_memory_store("stale_map_batch_upsert");
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

    let summary = store
        .upsert_documents(vec![make_document(a, "Doc A v2", "updated document")])
        .await
        .unwrap();
    assert_eq!(
        summary.failed, 0,
        "the batch upsert itself must succeed, got {summary:?}"
    );
    assert_eq!(summary.affected, 1);

    let fetched_b = store.get_document(ns, b).await.unwrap();
    assert!(
        fetched_b.is_some(),
        "a stale map entry for A must not let batch upsert delete B's row, got None"
    );
    assert_eq!(fetched_b.unwrap().body, "second document");

    let fetched_a = store.get_document(ns, a).await.unwrap();
    assert!(
        fetched_a.is_some(),
        "A must be inserted and correctly mapped by the batch upsert"
    );
    assert_eq!(fetched_a.unwrap().body, "updated document");
}

/// `delete_document_dml`'s unmanaged (no-writer-task) path wraps the FTS-row
/// delete and the map-row delete in one explicit `BEGIN IMMEDIATE` / `COMMIT`
/// / `ROLLBACK` block (`Fts5TextSearch::delete_document`'s fallback, used
/// when `current_writer_task` returns `None`) so a failure between the two
/// statements rolls both back together instead of leaving the FTS row
/// deleted while its map row survives. Forces that failure via the
/// `delete_dml_test_seam` for this test's own `(ns, a)` key and asserts on
/// the raw rows that neither statement's effect was left half-committed,
/// then disarms the seam and confirms a real delete still removes both rows.
///
/// No `#[serial_test::serial]` needed: the seam only fires for the armed
/// `(namespace, subject_id)` key, so a concurrent test operating on a
/// different key (e.g. `test_delete_document`) is unaffected even while this
/// test's seam is armed.
#[tokio::test]
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

    struct ResetSeamOnDrop {
        namespace: String,
        subject_id: Uuid,
    }
    impl Drop for ResetSeamOnDrop {
        fn drop(&mut self) {
            delete_dml_test_seam::disarm(&self.namespace, self.subject_id);
        }
    }
    let reset_seam = ResetSeamOnDrop {
        namespace: ns.to_string(),
        subject_id: a,
    };
    delete_dml_test_seam::arm(ns, a);

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

/// The seam's targets are stored in a shared set, not a single `Option`
/// slot -- two independently armed `(namespace, subject_id)` targets must
/// not clobber each other: arming a second target must not disturb the
/// first, disarming one target must remove only that target and leave the
/// other armed, and a delete against either target while armed must roll
/// back on its own key regardless of what else is armed at the time.
#[tokio::test]
async fn delete_dml_test_seam_isolates_two_independently_armed_targets() {
    let store = setup_memory_store("seam_isolation");
    let ns = "test_ns";
    let a = Uuid::new_v4();
    let c = Uuid::new_v4();
    store
        .upsert_document(make_document(a, "Doc A", "doc a body"))
        .await
        .unwrap();
    store
        .upsert_document(make_document(c, "Doc C", "doc c body"))
        .await
        .unwrap();

    delete_dml_test_seam::arm(ns, a);
    delete_dml_test_seam::arm(ns, c);
    assert!(
        delete_dml_test_seam::matches(ns, a),
        "arming a second target must not un-arm the first"
    );
    assert!(
        delete_dml_test_seam::matches(ns, c),
        "the second target must itself be armed"
    );

    // Both targets armed concurrently: each delete must roll back under its
    // own key, independent of the other's armed state.
    let (result_a, result_c) =
        tokio::join!(store.delete_document(ns, a), store.delete_document(ns, c));
    assert!(
        result_a.is_err(),
        "A's forced failure must fire while A is armed"
    );
    assert!(
        result_c.is_err(),
        "C's forced failure must fire while C is armed, independent of A"
    );
    assert!(
        store.get_document(ns, a).await.unwrap().is_some(),
        "A's rolled-back delete must leave A readable"
    );
    assert!(
        store.get_document(ns, c).await.unwrap().is_some(),
        "C's rolled-back delete must leave C readable"
    );

    // Disarm only A -- C's own target must survive untouched.
    delete_dml_test_seam::disarm(ns, a);
    assert!(
        !delete_dml_test_seam::matches(ns, a),
        "disarming A must remove exactly A's own target"
    );
    assert!(
        delete_dml_test_seam::matches(ns, c),
        "disarming A must leave C's independently armed target in place"
    );

    let deleted_a = store.delete_document(ns, a).await.unwrap();
    assert!(
        deleted_a,
        "with A disarmed, its delete must succeed for real"
    );
    let result_c_still_armed = store.delete_document(ns, c).await;
    assert!(
        result_c_still_armed.is_err(),
        "C must still force-fail: disarming A must not have disarmed C"
    );

    delete_dml_test_seam::disarm(ns, c);
    let deleted_c = store.delete_document(ns, c).await.unwrap();
    assert!(
        deleted_c,
        "with C disarmed, its delete must succeed for real"
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

include!("text_tests/document_crud_and_count_tests.rs");

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

include!("text_tests/substrate_and_record_kind_filter_tests.rs");

include!("text_tests/query_normalization_and_legacy_form_tests.rs");

include!("text_tests/trigram_literal_tests.rs");

include!("text_tests/operator_dollar_and_slash_tests.rs");

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

include!("text_tests/punctuated_literal_tests.rs");

include!("text_tests/score_metadata_and_namespace_tests.rs");

include!("text_tests/snippet_ranking_and_term_statistics_tests.rs");

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
        ..PoolConfig::for_test()
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
        ..PoolConfig::for_test()
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
        ..PoolConfig::for_test()
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
        ..PoolConfig::for_test()
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

#[path = "text_tests/bounded_ranking_tests.rs"]
mod bounded_ranking_tests;
