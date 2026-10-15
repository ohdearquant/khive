use super::{execute_statements, SqlStatement, SqlValue, SqliteError};
use rusqlite::{ffi, Connection};

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
        .unwrap();
    conn
}

fn statement(sql: &str) -> SqlStatement {
    SqlStatement::new(sql, vec![])
}

fn count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn caller_owns_transaction_and_affected_count_is_not_commit() {
    let conn = setup();
    for statements in [vec![], vec![statement("INSERT INTO t(id) VALUES (1)")]] {
        assert!(matches!(
            execute_statements(&conn, &statements),
            Err(SqliteError::InvalidData(_))
        ));
        assert_eq!(count(&conn), 0);
    }
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(execute_statements(&conn, &[]).unwrap(), 0);
    let statements = [
        SqlStatement::new(
            "INSERT INTO t VALUES (?1, ?2); -- tail",
            vec![
                SqlValue::Integer(1),
                SqlValue::Text("COMMIT; -- data".into()),
            ],
        ),
        SqlStatement::new("INSERT INTO t VALUES (?1, ?2)", vec![SqlValue::Integer(2)]),
        statement("UPDATE t SET value = 'absent' WHERE id = 99"),
    ];
    assert_eq!(execute_statements(&conn, &statements).unwrap(), 2);
    let values: Vec<Option<String>> = conn
        .prepare("SELECT value FROM t ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(values, [Some(String::from("COMMIT; -- data")), None]);
    assert!(!conn.is_autocommit());
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(&conn), 0);
    conn.execute_batch("BEGIN").unwrap();
    execute_statements(&conn, &[statement("INSERT INTO t(id) VALUES (3)")]).unwrap();
    conn.execute_batch("COMMIT").unwrap();
    assert_eq!(count(&conn), 1);
}

#[test]
fn explicit_control_and_executable_tails_refuse_before_writes() {
    let conn = setup();
    conn.execute_batch("BEGIN").unwrap();
    for control in [
        "BEGIN",
        "START TRANSACTION",
        "COMMIT",
        "END",
        "ROLLBACK",
        "ROLLBACK TO s",
        "SAVEPOINT s",
        "RELEASE s",
    ] {
        let statements = [
            statement("INSERT INTO t(id) VALUES (1)"),
            statement(&format!("\u{feff} ; -- prefix\n /* comment */ {control}")),
        ];
        let error = execute_statements(&conn, &statements).unwrap_err();
        assert!(matches!(error, SqliteError::InvalidData(message) if message.contains("index 1")));
        assert_eq!(count(&conn), 0);
        assert!(!conn.is_autocommit());
    }
    let statements = [
        statement("INSERT INTO t(id) VALUES (1)"),
        statement("INSERT INTO t(id) VALUES (2); COMMIT"),
    ];
    let error = execute_statements(&conn, &statements).unwrap_err();
    assert!(matches!(
        error,
        SqliteError::Rusqlite(rusqlite::Error::MultipleStatement)
    ));
    assert_eq!(count(&conn), 0);
    assert!(!conn.is_autocommit());
}

#[test]
fn native_bind_and_constraint_errors_leave_prior_writes_for_owner_rollback() {
    for (bad, expected) in [
        (
            SqlStatement::new(
                "INSERT INTO t(id) VALUES (?1)",
                vec![SqlValue::Integer(2), SqlValue::Integer(3)],
            ),
            ffi::SQLITE_RANGE,
        ),
        (
            statement("INSERT INTO t(id) VALUES (1)"),
            ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
        ),
    ] {
        let conn = setup();
        conn.execute_batch("BEGIN").unwrap();
        let statements = [
            statement("INSERT INTO t(id) VALUES (1)"),
            bad,
            statement("INSERT INTO t(id) VALUES (3)"),
        ];
        let error = execute_statements(&conn, &statements).unwrap_err();
        let SqliteError::Rusqlite(error) = error else {
            panic!("native error lost: {error}");
        };
        assert_eq!(error.sqlite_error().unwrap().extended_code, expected);
        assert_eq!(count(&conn), 1);
        assert!(!conn.is_autocommit());
        conn.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count(&conn), 0);
    }
}

#[test]
fn deferred_tail_refusal_precedes_that_element_but_not_earlier_ddl() {
    let conn = setup();
    conn.execute_batch("BEGIN").unwrap();
    let statements = [
        statement("CREATE TABLE later (id INTEGER)"),
        statement("INSERT INTO later VALUES (1); INSERT INTO t(id) VALUES (2)"),
    ];
    let error = execute_statements(&conn, &statements).unwrap_err();
    assert!(matches!(
        error,
        SqliteError::Rusqlite(rusqlite::Error::MultipleStatement)
    ));
    let later_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM later", [], |r| r.get(0))
        .unwrap();
    assert_eq!(later_count, 0);
    assert_eq!(count(&conn), 0);
    assert!(!conn.is_autocommit());
    conn.execute_batch("ROLLBACK").unwrap();
    assert!(conn.prepare("SELECT * FROM later").is_err());
}

#[test]
fn existing_authorizer_remains_effective() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let conn = setup();
    conn.execute_batch("BEGIN").unwrap();
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Delete { .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .unwrap();
    execute_statements(&conn, &[statement("INSERT INTO t(id) VALUES (1)")]).unwrap();
    let error = conn.execute("DELETE FROM t", []).unwrap_err();
    assert_eq!(
        error.sqlite_error().unwrap().extended_code,
        ffi::SQLITE_AUTH
    );
    assert_eq!(count(&conn), 1);
}

#[test]
fn every_sql_value_preserves_its_sqlite_representation() {
    use rusqlite::types::Value;
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE bound (value); BEGIN")
        .unwrap();
    let id = uuid::Uuid::from_u128(123);
    let timestamp = chrono::DateTime::from_timestamp_micros(1_234_567).unwrap();
    let values = [
        SqlValue::Null,
        SqlValue::Bool(true),
        SqlValue::Integer(-7),
        SqlValue::Float(0.25),
        SqlValue::Text("COMMIT; -- data".into()),
        SqlValue::Blob(vec![0, 255]),
        SqlValue::Json(serde_json::json!({"x": 1})),
        SqlValue::Uuid(id),
        SqlValue::Timestamp(timestamp),
    ];
    let statements: Vec<_> = values
        .into_iter()
        .map(|value| SqlStatement::new("INSERT INTO bound VALUES (?1)", vec![value]))
        .collect();
    assert_eq!(execute_statements(&conn, &statements).unwrap(), 9);
    conn.execute_batch("COMMIT").unwrap();
    let stored: Vec<Value> = conn
        .prepare("SELECT value FROM bound ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stored,
        [
            Value::Null,
            Value::Integer(1),
            Value::Integer(-7),
            Value::Real(0.25),
            Value::Text("COMMIT; -- data".into()),
            Value::Blob(vec![0, 255]),
            Value::Text(r#"{"x":1}"#.into()),
            Value::Text(id.to_string()),
            Value::Integer(1_234_567),
        ]
    );
}
