use super::*;
use crate::pool::PoolConfig;
use rusqlite::{Connection, StatementStatus};
use serde_json::{json, Value};

fn fixture() -> (Arc<ConnectionPool>, SqlGraphStore) {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(GRAPH_DDL).unwrap();
        writer
            .conn()
            .execute_batch(include_str!("../../sql/notes-ddl.sql"))
            .unwrap();
    }
    let store = SqlGraphStore::new_scoped(pool.clone(), false, "visible");
    (pool, store)
}

fn note(conn: &Connection, id: Uuid, created: i64, kind: &str, tags: Value, deleted: bool) {
    conn.execute(
        "INSERT INTO notes(id, namespace, kind, properties, created_at, updated_at, deleted_at) \
         VALUES (?1, 'note-owner', ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            id.to_string(),
            kind,
            json!({"tags": tags}).to_string(),
            created,
            deleted.then_some(created),
        ],
    )
    .unwrap();
}

fn edge(conn: &Connection, source: Uuid, target: Uuid, ns: &str, relation: &str, deleted: bool) {
    conn.execute(
        "INSERT INTO graph_edges(namespace, id, source_id, target_id, relation, \
         created_at, updated_at, deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, ?6)",
        rusqlite::params![
            ns,
            Uuid::new_v4().to_string(),
            source.to_string(),
            target.to_string(),
            relation,
            deleted.then_some(1),
        ],
    )
    .unwrap();
}

#[tokio::test]
async fn latest_annotation_filters_before_limit_and_keeps_edge_namespace_visibility() {
    let (pool, store) = fixture();
    let target = Uuid::from_u128(1);
    let receipt = Uuid::from_u128(2);
    let tied = Uuid::from_u128(3);
    let foreign = Uuid::from_u128(4);
    {
        let writer = pool.writer().unwrap();
        let conn = writer.conn();
        for id in [receipt, tied] {
            note(conn, id, 100, "observation", json!(["web.receipt"]), false);
            edge(conn, id, target, "visible", "annotates", false);
        }
        // Newer than the legitimate receipt: no arbitrary annotation window can
        // be truncated before checking its exact kind/tag and both live rows.
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..5_000 {
            let id = Uuid::from_u128(100 + i);
            note(conn, id, 1_000 + i as i64, "observation", json!([]), false);
            edge(conn, id, target, "visible", "annotates", false);
        }
        for (i, kind, tags, note_deleted, edge_deleted, relation) in [
            (
                0,
                "question",
                json!(["web.receipt"]),
                false,
                false,
                "annotates",
            ),
            (
                1,
                "observation",
                json!(["WEB.RECEIPT"]),
                false,
                false,
                "annotates",
            ),
            (
                2,
                "observation",
                json!("web.receipt"),
                false,
                false,
                "annotates",
            ),
            (
                3,
                "observation",
                json!({"value": "web.receipt"}),
                false,
                false,
                "annotates",
            ),
            (
                4,
                "observation",
                json!(["web.receipt"]),
                true,
                false,
                "annotates",
            ),
            (
                5,
                "observation",
                json!(["web.receipt"]),
                false,
                true,
                "annotates",
            ),
            (
                6,
                "observation",
                json!(["web.receipt"]),
                false,
                false,
                "references",
            ),
        ] {
            let id = Uuid::from_u128(10_000 + i);
            note(conn, id, 10_000, kind, tags, note_deleted);
            edge(conn, id, target, "visible", relation, edge_deleted);
        }
        note(
            conn,
            foreign,
            20_000,
            "observation",
            json!(["web.receipt"]),
            false,
        );
        edge(conn, foreign, target, "hidden", "annotates", false);
        let unrelated = Uuid::from_u128(20_000);
        note(
            conn,
            unrelated,
            30_000,
            "observation",
            json!(["web.receipt"]),
            false,
        );
        edge(
            conn,
            unrelated,
            Uuid::from_u128(30_000),
            "visible",
            "annotates",
            false,
        );
        conn.execute_batch("COMMIT").unwrap();
    }

    assert_eq!(
        store
            .latest_annotating_note(target, "observation", "web.receipt")
            .await
            .unwrap(),
        Some((receipt, 100)),
        "an exact receipt wins despite newer decoys; equal timestamps choose the smaller UUID"
    );
    assert_eq!(
        SqlGraphStore::new_scoped(pool.clone(), false, "hidden")
            .latest_annotating_note(target, "observation", "web.receipt")
            .await
            .unwrap(),
        Some((foreign, 20_000)),
        "annotation visibility does not add a note-namespace restriction to by-ID lookup"
    );
    assert_eq!(
        store
            .latest_annotating_note(Uuid::nil(), "observation", "web.receipt")
            .await
            .unwrap(),
        None
    );
}

fn query_steps(conn: &Connection, target: Uuid) -> (Uuid, i32) {
    let mut stmt = conn.prepare(LATEST_ANNOTATING_NOTE_SQL).unwrap();
    let id: String = stmt
        .query_row(
            rusqlite::params!["visible", target.to_string(), "observation", "web.receipt"],
            |row| row.get(0),
        )
        .unwrap();
    (
        Uuid::parse_str(&id).unwrap(),
        stmt.get_status(StatementStatus::VmStep),
    )
}

#[test]
fn latest_annotation_query_work_does_not_grow_with_older_receipt_history() {
    let (pool, _store) = fixture();
    let target = Uuid::from_u128(1);
    let newest = Uuid::from_u128(2);
    let writer = pool.writer().unwrap();
    let conn = writer.conn();
    note(
        conn,
        newest,
        100_000,
        "observation",
        json!(["web.receipt"]),
        false,
    );
    edge(conn, newest, target, "visible", "annotates", false);
    let (small_id, small_steps) = query_steps(conn, target);
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..10_000 {
        let id = Uuid::from_u128(100 + i);
        note(
            conn,
            id,
            i as i64,
            "observation",
            json!(["web.receipt"]),
            false,
        );
        edge(conn, id, target, "visible", "annotates", false);
    }
    conn.execute_batch("COMMIT; ANALYZE").unwrap();
    let (large_id, large_steps) = query_steps(conn, target);
    assert_eq!(small_id, newest);
    assert_eq!(large_id, newest);
    assert!(
        large_steps <= small_steps + 100,
        "older receipts must not be enumerated or sorted: small={small_steps}, large={large_steps}"
    );
}
