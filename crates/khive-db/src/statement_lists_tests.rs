use super::*;
use crate::sql_bridge::bind_params;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

fn database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../sql/notes-ddl.sql"))
        .unwrap();
    conn.execute_batch(include_str!("../sql/entities-ddl.sql"))
        .unwrap();
    conn
}

fn insert(conn: &Connection, table: &str, id: Uuid, properties: Option<&str>, deleted: bool) {
    conn.execute(
        &format!(
            "INSERT INTO {table}(id,namespace,kind,name,properties,created_at,updated_at,deleted_at) \
             VALUES (?1,'ns','head','fixture',?2,7,11,?3)"
        ),
        rusqlite::params![id.to_string(), properties, deleted.then_some(13)],
    )
    .unwrap();
}

fn snapshot(conn: &Connection, table: &str, id: Uuid) -> (Option<String>, i64, i64) {
    conn.query_row(
        &format!("SELECT properties,version,updated_at FROM {table} WHERE id=?1"),
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

fn properties(conn: &Connection, table: &str, id: Uuid) -> Value {
    serde_json::from_str(snapshot(conn, table, id).0.as_deref().unwrap()).unwrap()
}

fn execute(conn: &Connection, statement: &SqlStatement) -> rusqlite::Result<usize> {
    let mut prepared = conn.prepare(&statement.sql)?;
    bind_params(&mut prepared, &statement.params)?;
    prepared.raw_execute()
}

fn patch(
    conn: &Connection,
    table: &str,
    id: Uuid,
    expected: &[(&str, SqlValue)],
    set: &[(&str, SqlValue)],
) -> usize {
    let statements = property_cas_statements(table, id, expected, set).unwrap();
    assert_eq!(statements.len(), 1);
    assert!(statements[0].label.is_none());
    execute(conn, &statements[0]).unwrap()
}

#[test]
fn live_holder_query_keeps_scope_live_identity_binds_and_label() {
    let conn = database();
    let live = Uuid::new_v4();
    let deleted = Uuid::new_v4();
    for (id, namespace, kind, key, tombstone) in [
        (live, "ns", "head", "key", None),
        (deleted, "ns", "head", "key", Some(13)),
        (Uuid::new_v4(), "other", "head", "key", None),
        (Uuid::new_v4(), "ns", "other", "key", None),
        (Uuid::new_v4(), "ns", "head", "other", None),
    ] {
        conn.execute(
            "INSERT INTO notes(id,namespace,kind,key,created_at,updated_at,deleted_at) \
             VALUES (?1,?2,?3,?4,7,11,?5)",
            rusqlite::params![id.to_string(), namespace, kind, key, tombstone],
        )
        .unwrap();
    }
    let query = live_holder_read("ns", "head", "key");
    assert_eq!(query.sql, "SELECT id, version FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL");
    assert!(query.label.is_none());
    for (value, expected) in query.params.iter().zip(["ns", "head", "key"]) {
        assert!(matches!(value, SqlValue::Text(value) if value == expected));
    }
    assert_eq!(query.params.len(), 3);
    let labelled = query.labelled("existing-label");
    assert_eq!(labelled.label.as_deref(), Some("existing-label"));
    let mut prepared = conn.prepare(&labelled.sql).unwrap();
    bind_params(&mut prepared, &labelled.params).unwrap();
    let mut rows = prepared.raw_query();
    let row = rows.next().unwrap().unwrap();
    assert_eq!(row.get::<_, String>(0).unwrap(), live.to_string());
    assert_eq!(row.get::<_, i64>(1).unwrap(), 1);
    assert!(rows.next().unwrap().is_none());

    let absent = live_holder_read("ns", "head", "absent");
    let mut prepared = conn.prepare(&absent.sql).unwrap();
    bind_params(&mut prepared, &absent.params).unwrap();
    assert!(prepared.raw_query().next().unwrap().is_none());
}

#[test]
fn scalar_guards_match_together_and_lost_guards_leave_the_whole_row_unchanged() {
    let conn = database();
    for table in ["notes", "entities"] {
        let id = Uuid::new_v4();
        insert(
            &conn,
            table,
            id,
            Some(r#"{"status":"pending","priority":3,"keep":"yes"}"#),
            false,
        );
        let expected = [
            ("$.status", SqlValue::Text("pending".into())),
            ("$.priority", SqlValue::Integer(3)),
        ];
        let set = [
            ("$.status", SqlValue::Text("done".into())),
            ("$.attempt", SqlValue::Integer(4)),
        ];
        let before = snapshot(&conn, table, id);
        assert_eq!(patch(&conn, table, id, &expected, &set), 1);
        assert_eq!(
            properties(&conn, table, id),
            json!({"status":"done","priority":3,"keep":"yes","attempt":4})
        );
        let after = snapshot(&conn, table, id);
        assert_eq!(after.1, before.1 + 1);
        assert_eq!(after.2, before.2);
        assert_eq!(patch(&conn, table, id, &expected, &set), 0);
        assert_eq!(snapshot(&conn, table, id), after);
        let wrong_priority = [
            ("$.status", SqlValue::Text("done".into())),
            ("$.priority", SqlValue::Integer(99)),
        ];
        assert_eq!(patch(&conn, table, id, &wrong_priority, &set), 0);
        assert_eq!(snapshot(&conn, table, id), after);
        assert_eq!(patch(&conn, table, Uuid::new_v4(), &[], &set), 0);

        let dead = Uuid::new_v4();
        insert(
            &conn,
            table,
            dead,
            Some(r#"{"status":"pending","priority":3}"#),
            true,
        );
        let before = snapshot(&conn, table, dead);
        assert_eq!(patch(&conn, table, dead, &expected, &set), 0);
        assert_eq!(snapshot(&conn, table, dead), before);
    }
    let id = Uuid::new_v4();
    insert(&conn, "notes", id, Some("{}"), false);
    assert_eq!(
        patch(
            &conn,
            "notes",
            id,
            &[],
            &[("$.status", SqlValue::Text("done".into()))]
        ),
        1
    );
    let columns: (String, String, String, i64, Option<Vec<u8>>, Option<String>) = conn.query_row(
        "SELECT namespace,kind,status,updated_at,strict_due_key,due_source FROM notes WHERE id=?1",
        [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
    ).unwrap();
    assert_eq!(
        columns,
        ("ns".into(), "head".into(), "active".into(), 11, None, None)
    );
}

#[test]
fn json_scalar_guards_extract_values_and_follow_native_null_bool_numeric_equality() {
    let conn = database();
    for (stored, expected) in [
        (json!({"v":"quoted"}), SqlValue::Json(json!("quoted"))),
        (json!({"v":true}), SqlValue::Json(json!(true))),
        (json!({"v":true}), SqlValue::Bool(true)),
        (json!({"v":1}), SqlValue::Json(json!(1.0))),
        (json!({"v":1}), SqlValue::Float(1.0)),
        (json!({"v":false}), SqlValue::Integer(0)),
    ] {
        let id = Uuid::new_v4();
        insert(&conn, "notes", id, Some(&stored.to_string()), false);
        assert_eq!(
            patch(
                &conn,
                "notes",
                id,
                &[("$.v", expected)],
                &[("$.seen", SqlValue::Json(json!(true)))]
            ),
            1
        );
        assert_eq!(properties(&conn, "notes", id)["seen"], true);
    }
    for stored in ["{}", r#"{"v":null}"#] {
        for expected in [SqlValue::Null, SqlValue::Json(Value::Null)] {
            let id = Uuid::new_v4();
            insert(&conn, "notes", id, Some(stored), false);
            assert_eq!(
                patch(
                    &conn,
                    "notes",
                    id,
                    &[("$.v", expected)],
                    &[("$.seen", SqlValue::Integer(1))]
                ),
                1
            );
        }
    }
    let id = Uuid::new_v4();
    insert(&conn, "notes", id, Some(r#"{"v":"quoted"}"#), false);
    let before = snapshot(&conn, "notes", id);
    assert_eq!(
        patch(
            &conn,
            "notes",
            id,
            &[("$.v", SqlValue::Text("\"quoted\"".into()))],
            &[("$.seen", SqlValue::Integer(1))]
        ),
        0
    );
    assert_eq!(snapshot(&conn, "notes", id), before);
}

#[test]
fn json_set_preserves_json_types_and_ordinary_native_bindings() {
    let conn = database();
    let id = Uuid::new_v4();
    let uuid = Uuid::new_v4();
    let timestamp = chrono::DateTime::from_timestamp(2, 3_000).unwrap();
    insert(&conn, "notes", id, Some("{}"), false);
    let set = [
        ("$.object", SqlValue::Json(json!({"a":1}))),
        ("$.array", SqlValue::Json(json!([1, 2]))),
        ("$.string", SqlValue::Json(json!("text"))),
        ("$.boolean", SqlValue::Json(json!(true))),
        ("$.null", SqlValue::Json(Value::Null)),
        ("$.native_bool", SqlValue::Bool(true)),
        (
            "$.text",
            SqlValue::Text("{\"a\":1}; DROP TABLE notes".into()),
        ),
        ("$.uuid", SqlValue::Uuid(uuid)),
        ("$.time", SqlValue::Timestamp(timestamp)),
        ("$.float", SqlValue::Float(0.5)),
    ];
    assert_eq!(patch(&conn, "notes", id, &[], &set), 1);
    assert_eq!(
        properties(&conn, "notes", id),
        json!({"object":{"a":1},"array":[1,2],"string":"text","boolean":true,"null":null,"native_bool":1,"text":"{\"a\":1}; DROP TABLE notes","uuid":uuid.to_string(),"time":timestamp.timestamp_micros(),"float":0.5})
    );
    let kinds: (String, String, String, String, String, String) = conn.query_row(
        "SELECT json_type(properties,'$.object'),json_type(properties,'$.array'), \
         json_type(properties,'$.string'),json_type(properties,'$.boolean'), \
         json_type(properties,'$.null'),json_type(properties,'$.native_bool') FROM notes WHERE id=?1",
        [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
    ).unwrap();
    assert_eq!(
        kinds,
        (
            "object".into(),
            "array".into(),
            "text".into(),
            "true".into(),
            "null".into(),
            "integer".into()
        )
    );
    assert_eq!(
        patch(
            &conn,
            "notes",
            id,
            &[
                ("$.uuid", SqlValue::Uuid(uuid)),
                ("$.time", SqlValue::Timestamp(timestamp))
            ],
            &[("$.matched", SqlValue::Bool(true))]
        ),
        1
    );
}

#[test]
fn set_order_empty_set_and_empty_expected_have_the_declared_write_behavior() {
    let conn = database();
    let id = Uuid::new_v4();
    insert(&conn, "notes", id, Some("{}"), false);
    let before = snapshot(&conn, "notes", id);
    assert!(
        property_cas_statements("notes", id, &[("$.absent", SqlValue::Null)], &[])
            .unwrap()
            .is_empty()
    );
    assert_eq!(snapshot(&conn, "notes", id), before);
    let set = [
        ("$.branch", SqlValue::Json(json!({"a":0}))),
        ("$.branch.a", SqlValue::Integer(4)),
        ("$.branch.a", SqlValue::Integer(5)),
    ];
    let statements = property_cas_statements("notes", id, &[], &set).unwrap();
    assert!(matches!(statements[0].params[0], SqlValue::Uuid(value) if value == id));
    assert_eq!(execute(&conn, &statements[0]).unwrap(), 1);
    assert_eq!(properties(&conn, "notes", id), json!({"branch":{"a":5}}));
    assert_eq!(snapshot(&conn, "notes", id).1, before.1 + 1);
}

fn invalid_input(result: Result<Vec<SqlStatement>, StorageError>) {
    let error = result.unwrap_err();
    assert!(!error.to_string().contains("secret_marker"));
    assert!(
        matches!(error, StorageError::InvalidInput { capability: StorageCapability::Sql, operation, .. } if operation == "property_cas_statements")
    );
}

#[test]
fn invalid_table_path_and_unrepresentable_values_are_refused_even_for_empty_set() {
    let id = Uuid::new_v4();
    for table in [
        "graph_edges",
        "NOTES",
        "main.notes",
        "notes; DELETE FROM entities",
    ] {
        invalid_input(property_cas_statements(table, id, &[], &[]));
    }
    for path in [
        "status",
        "$.",
        "$.a..b",
        "$.a[0]",
        "$.a-b",
        "$.\"secret_marker\"",
        "$.a\0",
    ] {
        invalid_input(property_cas_statements(
            "notes",
            id,
            &[(path, SqlValue::Null)],
            &[],
        ));
        invalid_input(property_cas_statements(
            "notes",
            id,
            &[],
            &[(path, SqlValue::Null)],
        ));
    }
    for value in [
        SqlValue::Blob(b"secret_marker".to_vec()),
        SqlValue::Float(f64::NAN),
        SqlValue::Float(f64::INFINITY),
        SqlValue::Float(f64::NEG_INFINITY),
    ] {
        invalid_input(property_cas_statements(
            "notes",
            id,
            &[("$.a", value.clone())],
            &[],
        ));
        invalid_input(property_cas_statements("notes", id, &[], &[("$.a", value)]));
    }
    for value in [
        SqlValue::Json(json!({"a":"secret_marker"})),
        SqlValue::Json(json!(["secret_marker"])),
    ] {
        invalid_input(property_cas_statements("notes", id, &[("$.a", value)], &[]));
    }
}

#[test]
fn nonobjects_are_unchanged_sql_null_initializes_and_malformed_json_stays_native() {
    let conn = database();
    for stored in ["[]", "17", "\"text\"", "null"] {
        let id = Uuid::new_v4();
        insert(&conn, "entities", id, Some(stored), false);
        let before = snapshot(&conn, "entities", id);
        assert_eq!(
            patch(&conn, "entities", id, &[], &[("$.a", SqlValue::Integer(1))]),
            0
        );
        assert_eq!(snapshot(&conn, "entities", id), before);
    }
    let id = Uuid::new_v4();
    insert(&conn, "entities", id, None, false);
    assert_eq!(
        patch(
            &conn,
            "entities",
            id,
            &[("$.missing", SqlValue::Null)],
            &[("$.a", SqlValue::Integer(1))]
        ),
        1
    );
    assert_eq!(properties(&conn, "entities", id), json!({"a":1}));

    let malformed = Uuid::new_v4();
    insert(&conn, "entities", malformed, Some("{"), false);
    let before = snapshot(&conn, "entities", malformed);
    let statement =
        property_cas_statements("entities", malformed, &[], &[("$.a", SqlValue::Integer(1))])
            .unwrap()
            .remove(0);
    let error = execute(&conn, &statement).unwrap_err();
    assert!(
        matches!(&error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_ERROR)
    );
    assert!(error.to_string().contains("malformed JSON"), "{error}");
    assert_eq!(snapshot(&conn, "entities", malformed), before);
}

#[test]
fn caller_owned_transaction_rolls_back_the_patch_when_a_later_write_fails() {
    let mut conn = database();
    let id = Uuid::new_v4();
    insert(
        &conn,
        "entities",
        id,
        Some(r#"{"status":"pending"}"#),
        false,
    );
    let before = snapshot(&conn, "entities", id);
    let statements = property_cas_statements(
        "entities",
        id,
        &[("$.status", SqlValue::Text("pending".into()))],
        &[("$.status", SqlValue::Text("done".into()))],
    )
    .unwrap();
    {
        let tx = conn.transaction().unwrap();
        assert_eq!(execute(&tx, &statements[0]).unwrap(), 1);
        let error = tx.execute("INSERT INTO entities(id,namespace,kind,name,created_at,updated_at) VALUES (?1,'ns','head','duplicate',7,11)", [id.to_string()]).unwrap_err();
        assert!(
            matches!(&error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation)
        );
        assert!(error.to_string().contains("entities.id"), "{error}");
    }
    assert_eq!(snapshot(&conn, "entities", id), before);
    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM entities WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(exists, Some(id.to_string()));
}

#[test]
fn entity_version_overflow_retains_the_native_guard_and_row() {
    let conn = database();
    let id = Uuid::new_v4();
    // Seed the otherwise unreachable overflow fixture without changing the
    // update guard, then restore the production insertion guard before use.
    conn.execute_batch("DROP TRIGGER entities_version_insert_guard")
        .unwrap();
    conn.execute(
        "INSERT INTO entities(id,namespace,kind,name,properties,created_at,updated_at,version) \
         VALUES (?1,'ns','head','overflow','{\"status\":\"pending\"}',7,11,?2)",
        rusqlite::params![id.to_string(), i64::MAX],
    )
    .unwrap();
    conn.execute_batch(include_str!("../sql/entities-ddl.sql"))
        .unwrap();
    let insert_error = conn
        .execute(
            "INSERT INTO entities(id,namespace,kind,name,created_at,updated_at,version) \
         VALUES (?1,'ns','head','invalid version',7,11,?2)",
            rusqlite::params![Uuid::new_v4().to_string(), i64::MAX],
        )
        .unwrap_err();
    assert!(insert_error
        .to_string()
        .contains("entity insert version must be 1"));
    let before = snapshot(&conn, "entities", id);
    let set = [("$.status", SqlValue::Text("done".into()))];
    assert_eq!(
        patch(
            &conn,
            "entities",
            id,
            &[("$.status", SqlValue::Text("other".into()))],
            &set
        ),
        0
    );
    assert_eq!(snapshot(&conn, "entities", id), before);
    let statement = property_cas_statements(
        "entities",
        id,
        &[("$.status", SqlValue::Text("pending".into()))],
        &set,
    )
    .unwrap()
    .remove(0);
    let error = execute(&conn, &statement).unwrap_err();
    assert!(
        matches!(&error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation)
    );
    assert!(
        error
            .to_string()
            .contains("entity update version must advance by exactly one"),
        "{error}"
    );
    assert_eq!(snapshot(&conn, "entities", id), before);
}
