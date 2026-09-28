//! Direct orphan-sweep DML tests using vector-shaped SQLite tables and an
//! instrumented entity view. The visit count measures registry evaluation,
//! not vec0 query plans or production latency.
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
const LIVE_ROWS: usize = 31;
const FIRST_BATCH: i64 = 400;
fn fixture(orphans: usize) -> (Connection, Arc<AtomicUsize>) {
    let conn = Connection::open_in_memory().expect("synthetic memory database");
    let visits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&visits);
    conn.create_scalar_function("count_live_id", 1, FunctionFlags::SQLITE_UTF8, move |ctx| {
        counted.fetch_add(1, Ordering::SeqCst);
        ctx.get::<String>(0)
    })
    .expect("install visit observer");
    conn.execute_batch(
        "CREATE TABLE fixture_live_entities(id TEXT PRIMARY KEY NOT NULL, deleted_at TEXT);
         CREATE VIEW entities AS
           SELECT count_live_id(id) AS id, deleted_at FROM fixture_live_entities;
         CREATE TABLE notes(id TEXT PRIMARY KEY NOT NULL, deleted_at TEXT);
         CREATE TABLE knowledge_atoms(id TEXT PRIMARY KEY NOT NULL, deleted_at TEXT);
         CREATE TABLE fixture_vectors(
           subject_id TEXT PRIMARY KEY NOT NULL, namespace TEXT NOT NULL,
           kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL);
         CREATE TABLE ann_write_log(
           seq INTEGER PRIMARY KEY AUTOINCREMENT, namespace TEXT NOT NULL,
           embedding_model TEXT NOT NULL, kind TEXT NOT NULL, field TEXT NOT NULL,
           subject_id TEXT NOT NULL, op TEXT NOT NULL);",
    )
    .expect("synthetic schema");
    for i in 0..LIVE_ROWS {
        conn.execute(
            "INSERT INTO fixture_live_entities(id) VALUES (?1)",
            [format!("live-{i:06}")],
        )
        .unwrap();
    }
    for i in 0..orphans {
        insert_vector(&conn, &format!("orphan-{i:06}"), "ns:fixture");
    }
    assert_eq!(visits.load(Ordering::SeqCst), 0);
    (conn, visits)
}
fn insert_vector(conn: &Connection, id: &str, namespace: &str) {
    conn.execute(
        "INSERT INTO fixture_vectors VALUES (?1, ?2, 'entity', 'body', 'fixture_model')",
        [id, namespace],
    )
    .unwrap();
}
fn scalar(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}
fn measured_sweep(orphans: usize) -> usize {
    let (conn, visits) = fixture(orphans);
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let report = super::orphan_sweep_dml(
        &conn,
        "fixture_vectors",
        None,
        None,
        None,
        orphans as i64,
        false,
    )
    .expect("production DML on diagnostic schema");
    assert_eq!(report.deleted, orphans as u64);
    tx.commit().unwrap();
    assert_eq!(scalar(&conn, "SELECT count(*) FROM fixture_vectors"), 0);
    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM ann_write_log"),
        orphans as i64
    );
    visits.load(Ordering::SeqCst)
}
// The live registry is independent of the number of delete batches.
#[test]
fn live_registry_visits_do_not_multiply_with_delete_batches() {
    let one_batch = measured_sweep(FIRST_BATCH as usize);
    let four_batches = measured_sweep((FIRST_BATCH * 4) as usize);
    assert!(
        one_batch >= LIVE_ROWS,
        "observer must see the live-set evaluation"
    );
    eprintln!("live_id_visits: one_batch={one_batch}, four_batches={four_batches}");
    assert!(
        four_batches <= one_batch,
        "unrelated live-registry visits multiplied with the batch count: \
         one_batch={one_batch}, four_batches={four_batches}"
    );
}
// A multiset cardinality check catches duplicate log rows.
#[test]
fn full_and_partial_batches_log_each_deleted_subject_exactly_once() {
    let (conn, _visits) = fixture(405);
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let report =
        super::orphan_sweep_dml(&conn, "fixture_vectors", None, None, None, 403, false).unwrap();
    tx.commit().unwrap();
    assert_eq!(report.deleted, 403);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM fixture_vectors"), 2);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM ann_write_log"), 403);
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM (SELECT subject_id FROM ann_write_log \
                       GROUP BY subject_id HAVING count(*) <> 1)"
        ),
        0
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM ann_write_log l JOIN fixture_vectors v \
                       ON v.subject_id = l.subject_id"
        ),
        0
    );
}
// This covers the DML with the same RAII transaction primitive used by the direct
// route, not the separate production WriterTask scheduler/transaction wrapper.
#[test]
fn late_log_failure_rolls_back_prior_batch_deletes_and_logs() {
    let (conn, _visits) = fixture(405);
    conn.execute_batch(
        "CREATE TRIGGER abort_later_batch BEFORE INSERT ON ann_write_log
         WHEN (SELECT count(*) FROM ann_write_log) >= 400
         BEGIN SELECT RAISE(ABORT, 'late_log_failure'); END;",
    )
    .unwrap();
    {
        let _tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let error = super::orphan_sweep_dml(&conn, "fixture_vectors", None, None, None, 403, false)
            .expect_err("second batch must reach the injected failure");
        assert!(error.to_string().contains("late_log_failure"));
        assert_eq!(scalar(&conn, "SELECT count(*) FROM fixture_vectors"), 5);
        assert_eq!(scalar(&conn, "SELECT count(*) FROM ann_write_log"), 400);
    }
    assert_eq!(scalar(&conn, "SELECT count(*) FROM fixture_vectors"), 405);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM ann_write_log"), 0);
}
#[test]
fn dry_run_does_not_delete_or_append_logs() {
    let (conn, _visits) = fixture(405);
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let report =
        super::orphan_sweep_dml(&conn, "fixture_vectors", None, None, None, 403, true).unwrap();
    tx.commit().unwrap();
    assert_eq!(report.would_delete, 405);
    assert_eq!(report.deleted, 0);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM fixture_vectors"), 405);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM ann_write_log"), 0);
}
#[test]
fn required_live_table_absence_refuses_before_deletion() {
    let (conn, _visits) = fixture(5);
    conn.execute_batch("DROP TABLE knowledge_atoms").unwrap();
    {
        let _tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        super::orphan_sweep_dml(&conn, "fixture_vectors", None, None, None, 5, false)
            .expect_err("missing live-subject table must fail closed");
    }
    assert_eq!(scalar(&conn, "SELECT count(*) FROM fixture_vectors"), 5);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM ann_write_log"), 0);
}
#[test]
fn namespace_filter_and_live_records_remain_protected() {
    let (conn, _visits) = fixture(405);
    insert_vector(&conn, "live-000000", "ns:fixture");
    insert_vector(&conn, "outside-orphan", "outside");
    conn.execute("INSERT INTO notes(id) VALUES ('live-note')", [])
        .unwrap();
    conn.execute("INSERT INTO knowledge_atoms(id) VALUES ('live-atom')", [])
        .unwrap();
    insert_vector(&conn, "live-note", "ns:fixture");
    insert_vector(&conn, "live-atom", "ns:fixture");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let report = super::orphan_sweep_dml(
        &conn,
        "fixture_vectors",
        Some(r#"["ns:fixture"]"#),
        None,
        None,
        403,
        false,
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(report.scanned, 408);
    assert_eq!(report.would_delete, 405);
    assert_eq!(report.deleted, 403);
    for id in ["live-000000", "live-note", "live-atom", "outside-orphan"] {
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM fixture_vectors WHERE subject_id=?1)",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(present, "protected subject {id} disappeared");
    }
}
