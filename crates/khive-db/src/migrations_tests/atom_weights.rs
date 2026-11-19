use super::*;
use rusqlite::types::Value;

const NAME: &str = "atom_weight_tables";
// Frozen pre-V56 schema, kept inside this package so published khive-db tests
// do not require the sibling retrieval source tree.
const LEGACY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS atom_weights (
    namespace  TEXT NOT NULL,
    atom_id    TEXT NOT NULL,
    weight     REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    version    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (namespace, atom_id)
);
CREATE TABLE IF NOT EXISTS weight_events (
    namespace    TEXT NOT NULL,
    atom_id      TEXT NOT NULL,
    delta        REAL NOT NULL,
    weight_after REAL NOT NULL,
    channel      TEXT NOT NULL,
    eta          REAL NOT NULL,
    event_id     TEXT,
    context_id   TEXT,
    ts           INTEGER NOT NULL
);
"#;

fn schema(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    conn.prepare(
        "SELECT type, name, sql FROM sqlite_schema \
         WHERE tbl_name IN ('atom_weights', 'weight_events') ORDER BY type, name",
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
    // Use the real admitted units, including application-assisted conversions.
    // Deleting a tail ledger row from a current database is not an upgrade fixture.
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
fn atom_weight_migration_fresh_schema_matches_legacy_layout() {
    let legacy = open_memory();
    legacy.execute_batch(LEGACY_SCHEMA).unwrap();

    let mut fresh = open_memory();
    assert_eq!(run_migrations(&mut fresh).unwrap(), latest_schema_version());
    assert!(table_exists(&fresh, "atom_weights"));
    assert!(table_exists(&fresh, "weight_events"));
    assert_eq!(schema(&fresh), schema(&legacy));
}

#[test]
fn atom_weight_migration_upgrades_previous_tail_and_preserves_legacy_rows() {
    for preexisting in [false, true] {
        let mut conn = open_memory();
        let previous = historical_prefix(&mut conn);
        assert_eq!(read_schema_version(&conn).unwrap(), previous);
        assert!(!table_exists(&conn, "atom_weights"));
        assert!(!table_exists(&conn, "weight_events"));

        if preexisting {
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
            conn.execute_batch(
                "INSERT INTO atom_weights (namespace, atom_id, weight, updated_at, version) \
                 VALUES ('alpha', 'same-atom', 1.25, 1234, 7), \
                        ('beta', 'same-atom', 2.5, 2345, 11); \
                 INSERT INTO weight_events \
                     (rowid, namespace, atom_id, delta, weight_after, channel, eta, \
                      event_id, context_id, ts) \
                 VALUES (17, 'alpha', 'same-atom', -0.25, 1.25, 'explicit', 0.1, \
                         NULL, 'context-a', 3456), \
                        (42, 'beta', 'same-atom', 1.5, 2.5, 'ground_truth', 0.5, \
                         'event-b', NULL, 4567);",
            )
            .unwrap();
        }
        let before = preexisting.then(|| {
            (
                schema(&conn),
                rows(&conn, "SELECT rowid, * FROM atom_weights ORDER BY rowid"),
                rows(&conn, "SELECT rowid, * FROM weight_events ORDER BY rowid"),
            )
        });

        assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
        assert!(table_exists(&conn, "atom_weights"));
        assert!(table_exists(&conn, "weight_events"));
        let after = (
            schema(&conn),
            rows(&conn, "SELECT rowid, * FROM atom_weights ORDER BY rowid"),
            rows(&conn, "SELECT rowid, * FROM weight_events ORDER BY rowid"),
        );
        if let Some(before) = before {
            assert_eq!(
                after, before,
                "legacy schema, rowids and all values must survive"
            );
        } else {
            assert!(after.1.is_empty());
            assert!(after.2.is_empty());
        }
        assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
        assert_eq!(
            (
                schema(&conn),
                rows(&conn, "SELECT rowid, * FROM atom_weights ORDER BY rowid"),
                rows(&conn, "SELECT rowid, * FROM weight_events ORDER BY rowid"),
            ),
            after,
            "reopening an already migrated database must preserve schema and data"
        );
    }
}
