use super::*;
use rusqlite::types::Value;

const NAME: &str = "retrieval_snapshot_tables";
// Frozen pre-V57 schema, kept inside this package so published khive-db tests
// do not require the sibling retrieval source tree.
const LEGACY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS retrieval_snapshots (
    namespace   TEXT NOT NULL,
    index_type  TEXT NOT NULL,
    snapshot    BLOB NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (namespace, index_type)
);

CREATE INDEX IF NOT EXISTS idx_retrieval_snapshots_namespace
    ON retrieval_snapshots(namespace);
"#;

fn schema(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    conn.prepare(
        "SELECT type, name, sql FROM sqlite_schema \
         WHERE tbl_name = 'retrieval_snapshots' ORDER BY type, name",
    )
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = conn.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns)
                .map(|column| row.get::<_, Value>(column))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn historical_prefix(conn: &mut Connection) -> u32 {
    let next = MIGRATIONS
        .iter()
        .find(|migration| migration.name == NAME)
        .unwrap();
    let admission = WriteAdmission::for_migration_policy(None, &migration_test_policy()).unwrap();
    let mut writes = RawMigrationTransactions::new(conn, &admission);
    writes
        .admitted(|conn| bootstrap_migration_ledger(conn, &admission))
        .unwrap();
    // Apply the real historical units, including application-assisted conversions.
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version < next.version)
    {
        let step = writes
            .admitted(|conn| {
                apply_versioned_migration(conn, migration, &admission, latest_schema_version())
            })
            .unwrap();
        assert!(matches!(step, MigrationStep::Applied));
    }
    next.version - 1
}

#[test]
fn retrieval_snapshot_migration_fresh_schema_matches_legacy_layout() {
    let legacy = open_memory();
    legacy.execute_batch(LEGACY_SCHEMA).unwrap();

    let mut fresh = open_memory();
    assert_eq!(run_migrations(&mut fresh).unwrap(), latest_schema_version());
    assert!(table_exists(&fresh, "retrieval_snapshots"));
    assert_eq!(schema(&fresh), schema(&legacy));
    assert_eq!(
        rows(&fresh, "PRAGMA table_info(retrieval_snapshots)"),
        rows(&legacy, "PRAGMA table_info(retrieval_snapshots)"),
        "column types, nullability, defaults and primary-key order must match"
    );
}

#[test]
fn retrieval_snapshot_migration_upgrades_previous_tail_and_preserves_legacy_rows() {
    for preexisting in [false, true] {
        let mut conn = open_memory();
        let previous = historical_prefix(&mut conn);
        assert_eq!(previous, 56);
        assert_eq!(read_schema_version(&conn).unwrap(), previous);
        assert!(!table_exists(&conn, "retrieval_snapshots"));

        if preexisting {
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
            conn.execute_batch(
                "INSERT INTO retrieval_snapshots \
                    (rowid, namespace, index_type, snapshot, created_at) \
                 VALUES (17, 'alpha', 'bm25', X'00FF807F', 1234), \
                        (42, 'alpha', 'hnsw', X'', 0), \
                        (73, 'beta', 'bm25', X'FE0001', 9223372036854775807);",
            )
            .unwrap();
        }
        let snapshot = |conn: &Connection| {
            (
                schema(conn),
                rows(conn, "PRAGMA table_info(retrieval_snapshots)"),
                rows(
                    conn,
                    "SELECT rowid, * FROM retrieval_snapshots ORDER BY rowid",
                ),
            )
        };
        let before = preexisting.then(|| snapshot(&conn));

        assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
        assert!(table_exists(&conn, "retrieval_snapshots"));
        let after = snapshot(&conn);
        if let Some(before) = before {
            assert_eq!(
                after, before,
                "legacy schema, index, rowids, BLOB bytes and timestamps must survive"
            );
        } else {
            assert!(after.2.is_empty());
        }
        assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
        assert_eq!(
            snapshot(&conn),
            after,
            "reopening an already migrated database must preserve schema and data"
        );
    }
}
