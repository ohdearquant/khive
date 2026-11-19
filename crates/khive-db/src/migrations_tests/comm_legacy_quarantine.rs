use super::*;
use rusqlite::types::Value;

const NAME: &str = "comm_legacy_quarantine_index";
const INDEX: &str = "idx_comm_quarantine_legacy_expiry";

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
    for migration in MIGRATIONS
        .iter()
        .take_while(|migration| migration.name != NAME)
    {
        let step = writes
            .admitted(|conn| {
                apply_versioned_migration(conn, migration, &admission, latest_schema_version())
            })
            .unwrap();
        assert!(matches!(step, MigrationStep::Applied));
    }
}

fn index_schema(conn: &Connection) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='index' AND name=?1",
        [INDEX],
        |row| row.get(0),
    )
    .expect("legacy quarantine index must be installed by core migrations")
}

#[test]
fn legacy_quarantine_index_upgrade_preserves_rows_and_replays_without_schema_changes() {
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.name == NAME)
        .unwrap();
    assert_eq!(migration.version, 59);
    let mut fresh = open_memory();
    run_migrations(&mut fresh).unwrap();
    let expected_schema = index_schema(&fresh);

    let mut upgraded = open_memory();
    historical_prefix(&mut upgraded);
    assert_eq!(
        read_schema_version(&upgraded).unwrap(),
        migration.version - 1
    );
    upgraded
        .execute_batch(
            r#"
        INSERT INTO notes(id, namespace, kind, properties, created_at, updated_at,
                          expires_at, deleted_at)
        VALUES ('live', 'local', 'message',
                '{"channel_kind":"email","quarantined":true}', 1, 2, 3, NULL),
               ('tombstone', 'local', 'message',
                '{"channel_kind":"email","quarantined":"true","channel_slug":" "}',
                4, 5, NULL, 6),
               ('ordinary', 'other', 'observation', '{"preserve":[1,2]}',
                7, 8, NULL, NULL);
    "#,
        )
        .unwrap();
    let before_notes = rows(&upgraded, "SELECT * FROM notes ORDER BY id");
    let before_ledger = rows(
        &upgraded,
        "SELECT * FROM _schema_migrations ORDER BY version",
    );
    run_migrations(&mut upgraded).unwrap();
    assert_eq!(index_schema(&upgraded), expected_schema);
    assert_eq!(
        rows(&upgraded, "SELECT * FROM notes ORDER BY id"),
        before_notes
    );
    assert_eq!(
        rows(
            &upgraded,
            "SELECT * FROM _schema_migrations WHERE version<59 ORDER BY version"
        ),
        before_ledger
    );
    assert_eq!(
        rows(
            &upgraded,
            "SELECT version,name FROM _schema_migrations WHERE version=59"
        ),
        vec![vec![Value::Integer(59), Value::Text(NAME.into())]]
    );

    let schema_version: i64 = upgraded
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .unwrap();
    for replay in [false, true] {
        if replay {
            upgraded
                .execute("DELETE FROM _schema_migrations WHERE version>=59", [])
                .unwrap();
        }
        run_migrations(&mut upgraded).unwrap();
        assert_eq!(
            upgraded
                .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            schema_version,
            "reopen and ledger-tail replay must retain the existing index"
        );
        assert_eq!(index_schema(&upgraded), expected_schema);
        assert_eq!(
            rows(&upgraded, "SELECT * FROM notes ORDER BY id"),
            before_notes
        );
        assert!(upgraded.is_autocommit());
    }
}
