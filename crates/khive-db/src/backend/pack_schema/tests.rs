use super::*;
use crate::StorageBackend;

const ADDITIONS: &[PackColumnAddition] = &[
    PackColumnAddition {
        table: "upgrade_records",
        column: "revision",
        affinity: PackColumnAffinity::Text,
    },
    PackColumnAddition {
        table: "upgrade_records",
        column: "invalidated_at",
        affinity: PackColumnAffinity::Integer,
    },
];

const PLAN: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS upgrade_records (\
       id INTEGER PRIMARY KEY, saved TEXT, revision TEXT, invalidated_at INTEGER)",
    "CREATE TRIGGER IF NOT EXISTS upgrade_invalidate AFTER INSERT ON upgrade_records \
       WHEN NEW.revision IS NULL AND NEW.invalidated_at IS NULL BEGIN \
         UPDATE upgrade_records SET invalidated_at = 23 WHERE id = NEW.id; \
       END; \
     UPDATE upgrade_records SET invalidated_at = 23 \
       WHERE revision IS NULL AND invalidated_at IS NULL;",
];

fn seed(statements: &[&'static str]) -> StorageBackend {
    let backend = StorageBackend::memory().unwrap();
    backend.apply_pack_ddl_statements(statements).unwrap();
    backend
}

fn column_count(backend: &StorageBackend, column: &str) -> i64 {
    backend
        .pool()
        .reader()
        .unwrap()
        .conn()
        .query_row(
            "SELECT count(*) FROM pragma_table_xinfo('upgrade_records', 'main') WHERE name = ?1",
            [column],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn pack_column_upgrades_create_fresh_schema_and_reapply() {
    let backend = StorageBackend::memory().unwrap();
    backend
        .apply_pack_ddl_statements_with_columns(PLAN, ADDITIONS)
        .unwrap();
    backend
        .apply_pack_ddl_statements(&[
            "INSERT INTO upgrade_records VALUES (1, 'new row', NULL, NULL)",
        ])
        .unwrap();
    backend
        .apply_pack_ddl_statements_with_columns(PLAN, ADDITIONS)
        .unwrap();

    let reader = backend.pool().reader().unwrap();
    let row = reader
        .conn()
        .query_row(
            "SELECT saved, revision, invalidated_at FROM upgrade_records WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("new row".into(), None, 23));
}

#[test]
fn pack_column_upgrades_preserve_legacy_rows_and_run_backfill() {
    let backend = seed(&[
        "CREATE TABLE upgrade_records (id INTEGER PRIMARY KEY, saved TEXT)",
        "INSERT INTO upgrade_records VALUES (1, 'legacy row')",
    ]);
    backend
        .apply_pack_ddl_statements_with_columns(PLAN, ADDITIONS)
        .unwrap();

    let reader = backend.pool().reader().unwrap();
    let row = reader
        .conn()
        .query_row(
            "SELECT saved, revision, invalidated_at FROM upgrade_records WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("legacy row".into(), None, 23));
}

#[test]
fn pack_column_upgrades_complete_compatible_partial_schema_without_rebinding() {
    let backend = seed(&[
        "CREATE TABLE upgrade_records (id INTEGER PRIMARY KEY, saved TEXT, revision TEXT)",
        "INSERT INTO upgrade_records VALUES (1, 'pinned row', 'original revision')",
    ]);
    backend
        .apply_pack_ddl_statements_with_columns(PLAN, ADDITIONS)
        .unwrap();
    backend
        .apply_pack_ddl_statements(&["UPDATE upgrade_records SET invalidated_at = 77 WHERE id = 1"])
        .unwrap();
    backend
        .apply_pack_ddl_statements_with_columns(PLAN, ADDITIONS)
        .unwrap();

    let reader = backend.pool().reader().unwrap();
    let row = reader
        .conn()
        .query_row(
            "SELECT saved, revision, invalidated_at FROM upgrade_records WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("pinned row".into(), "original revision".into(), 77));
}

#[test]
fn pack_column_upgrades_reject_incompatible_existing_columns_and_rollback_additions() {
    const INCOMPATIBLE: &[&str] = &[
        "CREATE TABLE upgrade_records (id INTEGER, revision INTEGER)",
        "CREATE TABLE upgrade_records (id INTEGER, revision TEXT NOT NULL)",
        "CREATE TABLE upgrade_records (id INTEGER, revision TEXT DEFAULT NULL)",
        "CREATE TABLE upgrade_records (id INTEGER, revision TEXT PRIMARY KEY)",
        "CREATE TABLE upgrade_records (id INTEGER, revision TEXT GENERATED ALWAYS AS ('x') VIRTUAL)",
        "CREATE TABLE upgrade_records (id INTEGER, revision TEXT GENERATED ALWAYS AS ('x') STORED)",
    ];
    let reversed = [ADDITIONS[1], ADDITIONS[0]];
    for &ddl in INCOMPATIBLE {
        let backend = seed(&[ddl]);
        let error = backend
            .apply_pack_ddl_statements_with_columns(PLAN, &reversed)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incompatible pack schema column"),
            "{ddl}: {error}"
        );
        assert_eq!(column_count(&backend, "invalidated_at"), 0, "{ddl}");
    }
}

#[test]
fn pack_column_upgrades_reject_unsafe_identifiers_before_schema_sql() {
    for identifier in ["", "1field", "bad-name", "bad\"name", "main.records", "é"] {
        for addition in [
            PackColumnAddition {
                table: identifier,
                ..ADDITIONS[0]
            },
            PackColumnAddition {
                column: identifier,
                ..ADDITIONS[0]
            },
        ] {
            let backend = StorageBackend::memory().unwrap();
            let error = backend
                .apply_pack_ddl_statements_with_columns(PLAN, &[addition])
                .unwrap_err();
            assert!(error.to_string().contains("invalid pack schema identifier"));
            let reader = backend.pool().reader().unwrap();
            assert!(!table_exists(reader.conn(), "upgrade_records").unwrap());
        }
    }
}

#[test]
fn pack_column_upgrades_require_fresh_sql_to_create_every_declared_column() {
    const INCOMPLETE_PLANS: &[&[&str]] = &[
        &[],
        &["CREATE TABLE upgrade_records (id INTEGER PRIMARY KEY, revision TEXT)"],
        &["CREATE TABLE upgrade_records (id INTEGER PRIMARY KEY, revision INTEGER, invalidated_at INTEGER)"],
    ];
    for &plan in INCOMPLETE_PLANS {
        let backend = StorageBackend::memory().unwrap();
        let error = backend
            .apply_pack_ddl_statements_with_columns(plan, ADDITIONS)
            .unwrap_err();
        assert!(error.to_string().contains("pack schema"), "{error}");
        let reader = backend.pool().reader().unwrap();
        assert!(!table_exists(reader.conn(), "upgrade_records").unwrap());
    }
}

#[test]
fn pack_column_upgrades_rollback_schema_and_data_on_later_sql_failure() {
    let backend = seed(&[
        "CREATE TABLE upgrade_records (id INTEGER PRIMARY KEY, saved TEXT)",
        "INSERT INTO upgrade_records VALUES (1, 'original')",
    ]);
    let failing_plan = [
        PLAN[0],
        PLAN[1],
        "UPDATE upgrade_records SET saved = 'changed' WHERE id = 1",
        "CREATE INDEX upgrade_missing ON nonexistent_table(id)",
    ];
    let error = backend
        .apply_pack_ddl_statements_with_columns(&failing_plan, ADDITIONS)
        .unwrap_err();
    assert!(error.to_string().contains("nonexistent_table"));
    assert_eq!(column_count(&backend, "revision"), 0);
    assert_eq!(column_count(&backend, "invalidated_at"), 0);
    let reader = backend.pool().reader().unwrap();
    let saved: String = reader
        .conn()
        .query_row(
            "SELECT saved FROM upgrade_records WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(saved, "original");
    let triggers: i64 = reader
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'trigger' AND name = 'upgrade_invalidate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(triggers, 0);
}

#[test]
fn pack_column_upgrades_validate_post_sql_schema_and_rollback() {
    let backend = seed(&[
        "CREATE TABLE upgrade_records (id INTEGER PRIMARY KEY, saved TEXT)",
        "INSERT INTO upgrade_records VALUES (1, 'original')",
    ]);
    let error = backend
        .apply_pack_ddl_statements_with_columns(
            &["ALTER TABLE upgrade_records RENAME TO renamed_records"],
            ADDITIONS,
        )
        .unwrap_err();
    assert!(error.to_string().contains("did not create declared column"));
    assert_eq!(column_count(&backend, "revision"), 0);
    let reader = backend.pool().reader().unwrap();
    assert!(table_exists(reader.conn(), "upgrade_records").unwrap());
    assert!(!table_exists(reader.conn(), "renamed_records").unwrap());
}

#[test]
fn pack_column_upgrades_resolve_identifiers_with_sqlite_case_rules() {
    let backend = seed(&[
        "CREATE TABLE Upgrade_Records (id INTEGER PRIMARY KEY, saved TEXT, Revision text)",
        "INSERT INTO Upgrade_Records VALUES (1, 'original', 'pinned')",
    ]);
    backend
        .apply_pack_ddl_statements_with_columns(PLAN, ADDITIONS)
        .unwrap();
    let reader = backend.pool().reader().unwrap();
    let revision: String = reader
        .conn()
        .query_row(
            "SELECT revision FROM upgrade_records WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision, "pinned");
}
