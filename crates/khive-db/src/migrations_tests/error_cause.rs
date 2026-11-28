use super::*;

// SQLite's generic SQL error, reported for a statement that names a missing table.
const SQLITE_ERROR: i32 = 1;
// SQLite's result code for a file that does not hold a database.
const SQLITE_NOTADB: i32 = 26;

// A body that fails inside the migration transaction: the table does not exist.
const FAILING_BODY: &str = "INSERT INTO error_cause_missing_table (id) VALUES (1);";

// Applies every real migration up to and including `last_version`, each in its own
// admitted unit, so a test can start from a historical schema.
fn migrate_through(conn: &mut Connection, last_version: u32) {
    let admission = WriteAdmission::for_migration_policy(None, &migration_test_policy()).unwrap();
    let mut writes = RawMigrationTransactions::new(conn, &admission);
    writes
        .admitted(|conn| bootstrap_migration_ledger(conn, &admission))
        .unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= last_version)
    {
        let step = writes
            .admitted(|conn| {
                apply_versioned_migration(conn, migration, &admission, latest_schema_version())
            })
            .unwrap();
        assert!(matches!(step, MigrationStep::Applied));
    }
}

#[test]
fn failed_migration_body_keeps_its_typed_driver_cause() {
    let mut conn = open_memory();
    assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());

    let failing = VersionedMigration {
        version: latest_schema_version() + 1,
        name: "error_cause_failing_body",
        up: FAILING_BODY,
    };
    let admission = WriteAdmission::for_migration_policy(None, &migration_test_policy()).unwrap();
    let result = {
        let mut writes = RawMigrationTransactions::new(&mut conn, &admission);
        writes.admitted(|conn| {
            apply_versioned_migration(conn, &failing, &admission, latest_schema_version())
        })
    };
    let Err(error) = result else {
        panic!("a migration whose body fails must not apply");
    };

    // The text a caller saw before the cause was kept: the driver's own message.
    let driver_error = open_memory()
        .execute_batch(FAILING_BODY)
        .expect_err("the body names a table the schema does not have");
    let expected = format!("migration v{} failed: {}", failing.version, driver_error);

    assert!(
        error.migration_cause().is_some(),
        "a failed migration body must keep its typed cause: {error:?}"
    );
    assert!(
        matches!(error.migration_cause(), Some(SqliteError::Rusqlite(_))),
        "the kept cause must be the driver error: {error:?}"
    );
    assert_eq!(
        error.native_extended_code(),
        Some(SQLITE_ERROR),
        "the native result code must survive the migration wrapper: {error:?}"
    );
    assert!(
        std::error::Error::source(&error).is_some(),
        "the kept cause must be reachable through the standard source chain"
    );
    assert_eq!(error.to_string(), expected);
    assert!(
        !error.to_string().contains("sqlite error:"),
        "the message must stay the bare driver text: {error}"
    );
}

#[test]
fn non_database_store_reports_the_native_not_a_database_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("not-a-database.db");
    std::fs::write(&path, b"not a sqlite database\n".repeat(64)).expect("write fixture");
    let mut conn = Connection::open(&path).expect("opening does not read the file");

    let error = run_migrations(&mut conn).expect_err("a non-database file must refuse migration");

    // The ledger bootstrap reads the file before any versioned migration begins,
    // so the refusal does not come from a migration unit.
    assert!(
        !matches!(error, SqliteError::Migration { .. }),
        "the refusal precedes every migration unit: {error:?}"
    );
    assert_eq!(
        error.native_extended_code(),
        Some(SQLITE_NOTADB),
        "the native result code must reach the caller: {error:?}"
    );
}

#[test]
fn message_only_migration_failure_has_no_typed_cause() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 57);
    conn.execute_batch("ALTER TABLE proposals_open ADD COLUMN archived_at TEXT;")
        .unwrap();

    let error = run_migrations(&mut conn).expect_err("an incompatible archive column must refuse");

    let SqliteError::Migration {
        version,
        error: text,
        source,
    } = &error
    else {
        panic!("expected a migration refusal, got {error:?}");
    };
    assert_eq!(*version, 58);
    assert!(
        text.contains("incompatible proposals_open.archived_at"),
        "{text}"
    );
    assert!(
        source.is_none(),
        "a literal message has no underlying error"
    );
    assert!(error.migration_cause().is_none());
    assert_eq!(error.native_extended_code(), None);
}
