//! The trigger that keeps an entity's kind equal to the kind its keys are held
//! under (ADR-221 section 1).

use rusqlite::{params, Connection, OptionalExtension};

const REFUSAL: &str = "entity kind differs from the kind its keys are held under";

fn store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    super::ensure_entities_schema(&conn).unwrap();
    conn
}

fn entity(conn: &Connection, id: &str, kind: &str) {
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES (?1, 'local', ?2, ?1, 1, 1)",
        params![id, kind],
    )
    .unwrap();
}

fn key(conn: &Connection, id: &str, kind: &str, key: &str) {
    conn.execute(
        "INSERT INTO entity_keys (namespace, kind, key, entity_id, created_at) \
         VALUES ('local', ?1, ?2, ?3, 1)",
        params![kind, key, id],
    )
    .unwrap();
}

fn run(conn: &Connection, sql: &str, id: &str) -> rusqlite::Result<usize> {
    conn.execute(sql, params![id])
}

fn refused(conn: &Connection, sql: &str, id: &str) {
    let error = run(conn, sql, id).expect_err(sql);
    assert!(error.to_string().contains(REFUSAL), "{sql}: {error}");
}

fn row(conn: &Connection, id: &str) -> (String, Option<i64>) {
    conn.query_row(
        "SELECT kind, deleted_at FROM entities WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn released_at(conn: &Connection, id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT released_at FROM entity_keys WHERE entity_id = ?1",
        params![id],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
    .flatten()
}

#[test]
fn a_live_entity_holding_a_key_keeps_its_kind() {
    let conn = store();
    entity(&conn, "a", "concept");
    key(&conn, "a", "concept", "k");
    refused(
        &conn,
        "UPDATE entities SET version = version + 1, kind = 'person' WHERE id = ?1",
        "a",
    );
    assert_eq!(row(&conn, "a"), ("concept".into(), None));

    // Writing the same kind again, as a whole-row update does, passes.
    run(
        &conn,
        "UPDATE entities SET version = version + 1, kind = 'concept', name = 'renamed' WHERE id = ?1",
        "a",
    )
    .unwrap();

    // An entity holding no key changes kind freely.
    entity(&conn, "b", "concept");
    run(
        &conn,
        "UPDATE entities SET version = version + 1, kind = 'person' WHERE id = ?1",
        "b",
    )
    .unwrap();
    assert_eq!(row(&conn, "b"), ("person".into(), None));
}

#[test]
fn a_whole_row_upsert_changing_the_kind_of_a_keyed_entity_is_refused() {
    let conn = store();
    entity(&conn, "a", "concept");
    key(&conn, "a", "concept", "k");
    refused(
        &conn,
        "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES (?1, 'local', 'person', 'a', 1, 2) \
         ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, updated_at = excluded.updated_at, version = entities.version + 1",
        "a",
    );
    assert_eq!(row(&conn, "a"), ("concept".into(), None));
}

#[test]
fn a_restore_under_another_kind_than_its_released_keys_is_refused() {
    let conn = store();
    entity(&conn, "a", "concept");
    key(&conn, "a", "concept", "k");
    run(
        &conn,
        "UPDATE entities SET version = version + 1, deleted_at = 5 WHERE id = ?1",
        "a",
    )
    .unwrap();
    assert_eq!(released_at(&conn, "a"), Some(5));

    // A deleted entity's kind may change; its keys stay released.
    run(
        &conn,
        "UPDATE entities SET version = version + 1, kind = 'person' WHERE id = ?1",
        "a",
    )
    .unwrap();
    refused(
        &conn,
        "UPDATE entities SET version = version + 1, deleted_at = NULL WHERE id = ?1",
        "a",
    );
    assert_eq!(row(&conn, "a"), ("person".into(), Some(5)));
    assert_eq!(released_at(&conn, "a"), Some(5));

    // Back under the keys' kind, the restore takes the key back.
    run(
        &conn,
        "UPDATE entities SET version = version + 1, kind = 'concept' WHERE id = ?1",
        "a",
    )
    .unwrap();
    run(
        &conn,
        "UPDATE entities SET version = version + 1, deleted_at = NULL WHERE id = ?1",
        "a",
    )
    .unwrap();
    assert_eq!(row(&conn, "a"), ("concept".into(), None));
    assert_eq!(released_at(&conn, "a"), None);
}
