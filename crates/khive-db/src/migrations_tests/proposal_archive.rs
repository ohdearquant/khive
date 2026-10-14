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
