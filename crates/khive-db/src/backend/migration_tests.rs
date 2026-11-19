#[test]
fn apply_schema_runs_migrations_idempotently() {
    static MIGRATIONS: &[crate::migrations::Migration] = &[crate::migrations::Migration {
        id: "001_init",
        up_sql: "CREATE TABLE IF NOT EXISTS schema_test (id TEXT PRIMARY KEY);",
        down_sql: None,
        is_already_applied: None,
    }];
    let plan = crate::migrations::ServiceSchemaPlan {
        service: "schema_test_svc",
        sqlite: MIGRATIONS,
        postgres: &[],
    };

    let backend = StorageBackend::memory().unwrap();
    backend.apply_schema(&plan).unwrap();
    backend.apply_schema(&plan).unwrap();

    let reader = backend.pool().reader().unwrap();
    let count: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_test'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
