use super::*;
use rusqlite::types::Value;

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        (0..columns)
            .map(|column| row.get(column))
            .collect::<rusqlite::Result<Vec<Value>>>()
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn historical_prefix(conn: &mut Connection) {
    let admission = WriteAdmission::for_migration_policy(None, &migration_test_policy()).unwrap();
    let mut writes = RawMigrationTransactions::new(conn, &admission);
    writes
        .admitted(|conn| bootstrap_migration_ledger(conn, &admission))
        .unwrap();
    for migration in MIGRATIONS.iter().filter(|migration| migration.version < 58) {
        let step = writes
            .admitted(|conn| {
                apply_versioned_migration(conn, migration, &admission, latest_schema_version())
            })
            .unwrap();
        assert!(matches!(step, MigrationStep::Applied));
    }
}

#[test]
fn proposal_archive_fresh_schema_and_registered_ordinal() {
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.name == "proposal_archive")
        .unwrap();
    assert_eq!(
        migration.version, 58,
        "ADR-046 allocation; V57 belongs to #5185"
    );
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    let column = rows(&conn, "SELECT type,\"notnull\",dflt_value FROM pragma_table_info('proposals_open') WHERE name='archived_at'");
    assert_eq!(
        column,
        vec![vec![
            Value::Text("INTEGER".into()),
            Value::Integer(0),
            Value::Null
        ]]
    );
    assert_eq!(
        rows(&conn, "SELECT COUNT(*) FROM proposals_open"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn proposal_archive_upgrade_and_reopen_preserve_every_old_value_and_event_byte() {
    let mut conn = open_memory();
    historical_prefix(&mut conn);
    assert_eq!(
        read_schema_version(&conn).unwrap(),
        57,
        "merge prerequisite #5185 must be present"
    );
    assert!(rows(
        &conn,
        "SELECT name FROM pragma_table_info('proposals_open') WHERE name='archived_at'"
    )
    .is_empty());
    conn.execute_batch(r#"
        INSERT INTO proposals_open
          (proposal_id,namespace,proposer,title,status,created_at,updated_at,expiry,last_decision,review_count,approve_count,reject_count)
        VALUES ('aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee','local','alice','applied title','applied',11,12,13,'approve',4,3,1),
               ('bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee','other','bob','rejected title','rejected',21,22,NULL,'reject',7,2,5);
        INSERT INTO events (id,namespace,verb,substrate,actor,outcome,kind,payload,created_at)
        VALUES ('cccccccc-bbbb-4ccc-8ddd-eeeeeeeeeeee','local','propose','entity','alice','success','proposal_created',
                '{ "proposal_id" : "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", "preserve" : [1, 2] }',9);
    "#).unwrap();
    let projection = "SELECT proposal_id,namespace,proposer,title,status,created_at,updated_at,expiry,last_decision,review_count,approve_count,reject_count FROM proposals_open ORDER BY proposal_id";
    let before_rows = rows(&conn, projection);
    let before_events = rows(&conn, "SELECT * FROM events ORDER BY id");
    run_migrations(&mut conn).unwrap();
    assert_archive_column(&conn);
    assert_archive_ledger(&conn);
    assert_eq!(rows(&conn, projection), before_rows);
    assert_eq!(
        rows(
            &conn,
            "SELECT archived_at FROM proposals_open ORDER BY proposal_id"
        ),
        vec![vec![Value::Null], vec![Value::Null]]
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM events ORDER BY id"),
        before_events
    );
    conn.execute(
        "UPDATE proposals_open SET archived_at=123 WHERE namespace='local'",
        [],
    )
    .unwrap();
    let archived = rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id");
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id"),
        archived
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM events ORDER BY id"),
        before_events
    );
}

fn archive_column(conn: &Connection) -> Vec<Vec<Value>> {
    rows(
        conn,
        "SELECT name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo('proposals_open') WHERE name='archived_at' COLLATE NOCASE",
    )
}

fn assert_archive_column(conn: &Connection) {
    assert_eq!(
        archive_column(conn),
        vec![vec![
            Value::Text("archived_at".into()),
            Value::Text("INTEGER".into()),
            Value::Integer(0),
            Value::Null,
            Value::Integer(0),
            Value::Integer(0),
        ]]
    );
}

fn assert_archive_ledger(conn: &Connection) {
    assert_eq!(
        rows(
            conn,
            "SELECT version,name FROM _schema_migrations WHERE version=58"
        ),
        vec![vec![
            Value::Integer(58),
            Value::Text("proposal_archive".into())
        ]]
    );
}

fn seed_compatibility_rows(conn: &Connection, explicit_marker: bool) {
    let (column, value) = if explicit_marker {
        (",archived_at", ",123")
    } else {
        ("", "")
    };
    conn.execute_batch(&format!(r#"
        INSERT INTO proposals_open
          (proposal_id,namespace,proposer,title,status,created_at,updated_at,expiry,last_decision,review_count,approve_count,reject_count{column})
        VALUES ('aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee','local','alice','applied title','applied',11,12,13,'approve',4,3,1{value}),
               ('bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee','other','bob','rejected title','rejected',21,22,NULL,'reject',7,2,5{value});
        INSERT INTO events (id,namespace,verb,substrate,actor,outcome,kind,payload,created_at)
        VALUES ('cccccccc-bbbb-4ccc-8ddd-eeeeeeeeeeee','local','propose','entity','alice','success','proposal_created',
                '{{ "proposal_id" : "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", "preserve" : [1, 2] }}',9);
    "#)).unwrap();
}

#[test]
fn proposal_archive_replay_preserves_existing_column_rows_and_event_bytes() {
    let mut conn = open_memory();
    historical_prefix(&mut conn);
    assert_eq!(read_schema_version(&conn).unwrap(), 57);
    assert!(archive_column(&conn).is_empty());
    seed_compatibility_rows(&conn, false);
    run_migrations(&mut conn).unwrap();
    assert_archive_column(&conn);
    assert_archive_ledger(&conn);
    conn.execute(
        "UPDATE proposals_open SET archived_at=123 WHERE namespace='local'",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(
            &conn,
            "SELECT archived_at FROM proposals_open ORDER BY proposal_id"
        ),
        vec![vec![Value::Integer(123)], vec![Value::Null]]
    );
    // Replay a contiguous tail even after later migrations are registered.
    assert_eq!(
        conn.execute("DELETE FROM _schema_migrations WHERE version>=58", [])
            .unwrap(),
        MIGRATIONS
            .iter()
            .filter(|migration| migration.version >= 58)
            .count()
    );
    assert_eq!(read_schema_version(&conn).unwrap(), 57);
    assert!(rows(
        &conn,
        "SELECT version FROM _schema_migrations WHERE version=58"
    )
    .is_empty());
    let proposals = rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id");
    let events = rows(&conn, "SELECT * FROM events ORDER BY id");
    let earlier_ledger = rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version");
    let column = archive_column(&conn);

    run_migrations(&mut conn).expect("compatible physical column must survive ledger replay");
    assert!(conn.is_autocommit());
    assert_archive_ledger(&conn);
    assert_eq!(archive_column(&conn), column);
    assert_eq!(
        rows(
            &conn,
            "SELECT * FROM _schema_migrations WHERE version<58 ORDER BY version"
        ),
        earlier_ledger
    );
    let complete_ledger = rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version");
    for _ in 0..2 {
        assert_eq!(
            rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id"),
            proposals
        );
        assert_eq!(rows(&conn, "SELECT * FROM events ORDER BY id"), events);
        assert_eq!(
            rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version"),
            complete_ledger
        );
        assert_eq!(archive_column(&conn), column);
        run_migrations(&mut conn).unwrap();
        assert!(conn.is_autocommit());
    }
    assert_eq!(
        rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version"),
        complete_ledger
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id"),
        proposals
    );
    assert_eq!(rows(&conn, "SELECT * FROM events ORDER BY id"), events);
    assert_eq!(archive_column(&conn), column);
}

fn assert_incompatible_archive_column(declaration: &str, generated: bool) {
    let mut conn = open_memory();
    historical_prefix(&mut conn);
    assert_eq!(read_schema_version(&conn).unwrap(), 57);
    assert!(archive_column(&conn).is_empty());
    assert_eq!(
        rows(&conn, "SELECT COUNT(*) FROM proposals_open"),
        vec![vec![Value::Integer(0)]]
    );
    conn.execute_batch(&format!(
        "ALTER TABLE proposals_open ADD COLUMN {declaration};"
    ))
    .unwrap();
    seed_compatibility_rows(&conn, !generated);
    let column = archive_column(&conn);
    assert_eq!(
        column.len(),
        1,
        "fixture must expose the incompatible column"
    );
    let proposals = rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id");
    let events = rows(&conn, "SELECT * FROM events ORDER BY id");
    let ledger = rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version");
    for _ in 0..2 {
        let error = run_migrations(&mut conn).expect_err("incompatible archive column must refuse");
        match error {
            SqliteError::Migration { version: 58, error } => {
                assert!(
                    error.contains("incompatible proposals_open.archived_at"),
                    "{error}"
                );
            }
            other => panic!("expected named V58 compatibility refusal, got {other:?}"),
        }
        assert!(conn.is_autocommit());
        assert!(rows(
            &conn,
            "SELECT version FROM _schema_migrations WHERE version=58"
        )
        .is_empty());
        assert_eq!(archive_column(&conn), column);
        assert_eq!(
            rows(&conn, "SELECT * FROM proposals_open ORDER BY proposal_id"),
            proposals
        );
        assert_eq!(rows(&conn, "SELECT * FROM events ORDER BY id"), events);
        assert_eq!(
            rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version"),
            ledger
        );
    }
}

#[test]
fn proposal_archive_incompatible_text_refuses() {
    assert_incompatible_archive_column("archived_at TEXT", false);
}

#[test]
fn proposal_archive_incompatible_not_null_refuses() {
    assert_incompatible_archive_column("archived_at INTEGER NOT NULL", false);
}

#[test]
fn proposal_archive_incompatible_default_zero_refuses() {
    assert_incompatible_archive_column("archived_at INTEGER DEFAULT 0", false);
}

#[test]
fn proposal_archive_incompatible_default_null_refuses() {
    assert_incompatible_archive_column("archived_at INTEGER DEFAULT NULL", false);
}

#[test]
fn proposal_archive_incompatible_generated_refuses() {
    assert_incompatible_archive_column(
        "archived_at INTEGER GENERATED ALWAYS AS (created_at) VIRTUAL",
        true,
    );
}
