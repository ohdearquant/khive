use super::*;
use crate::sql_bridge::execute_statements;
use rusqlite::{types::Value, Connection};
use std::collections::BTreeMap;

fn snapshot(conn: &Connection) -> Vec<BTreeMap<String, Value>> {
    let mut statement = conn.prepare("SELECT * FROM entities ORDER BY id").unwrap();
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    statement
        .query_map([], |row| {
            columns
                .iter()
                .enumerate()
                .map(|(i, name)| Ok((name.clone(), row.get::<_, Value>(i)?)))
                .collect::<rusqlite::Result<_>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn merge_tombstone_preserves_guards_provenance_and_owner_settlement() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(ENTITIES_DDL).unwrap();
    let namespace = "scope:'; DELETE FROM entities; --";
    let from_id = Uuid::from_u128(1);
    let into_id = Uuid::from_u128(2);
    let event_id = Uuid::from_u128(99);
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    for id in 1..=4 {
        let scope = if id == 4 { "other" } else { namespace };
        let mut entity = Entity::minimal(Uuid::from_u128(id), scope, "concept", "retained", 11, 12);
        entity.description = Some("retained description".into());
        entity.tags = vec!["retained tag".into()];
        entity.properties = Some(serde_json::json!({"retained": true}));
        if id == 3 {
            entity.deleted_at = Some(13);
            entity.merged_into = Some(into_id);
            entity.merge_event_id = Some(Uuid::from_u128(98));
        }
        execute_statements(&conn, &[entity_upsert_statement(&entity)]).unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    let before = snapshot(&conn);
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    for (scope, id) in [
        ("other", from_id),
        (namespace, Uuid::from_u128(5)),
        (namespace, Uuid::from_u128(3)),
        (namespace, Uuid::from_u128(4)),
    ] {
        let statement = entity_merge_tombstone_statement(scope, id, into_id, event_id, 42);
        assert_eq!(execute_statements(&conn, &[statement]).unwrap(), 0);
        assert_eq!(snapshot(&conn), before);
    }
    let statement = entity_merge_tombstone_statement(namespace, from_id, into_id, event_id, 42);
    assert_eq!(
        execute_statements(&conn, std::slice::from_ref(&statement)).unwrap(),
        1
    );
    let mut expected = before.clone();
    for (field, value) in [
        ("deleted_at", Value::Integer(42)),
        ("updated_at", Value::Integer(42)),
        ("version", Value::Integer(2)),
        ("merged_into", Value::Text(into_id.to_string())),
        ("merge_event_id", Value::Text(event_id.to_string())),
    ] {
        expected[0].insert(field.into(), value);
    }
    assert_eq!(snapshot(&conn), expected);
    assert!(!conn.is_autocommit());
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(snapshot(&conn), before);

    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(execute_statements(&conn, &[statement]).unwrap(), 1);
    conn.execute_batch("COMMIT").unwrap();
    assert_eq!(snapshot(&conn), expected);
}
