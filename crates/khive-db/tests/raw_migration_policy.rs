//! Public raw migration entry points run as a dependency, without cfg(test).

use khive_db::migrations::{
    apply_schema_plan_with_policy, finalize_attachment_cutover_with_policy, latest_schema_version,
    run_migrations, run_migrations_with_policy, stage_attachment_cutover_with_policy, Migration,
    MigrationWritePolicy, ServiceSchemaPlan,
};
use khive_db::{DiskGuardEnvironment, SqliteError};
use khive_storage::CapacityUnavailablePhase;
use rusqlite::Connection;

const SCHEMA: ServiceSchemaPlan = ServiceSchemaPlan {
    service: "explicit-admission-fixture",
    sqlite: &[Migration {
        id: "create-fixture",
        up_sql: "CREATE TABLE explicit_policy_marker (id INTEGER PRIMARY KEY)",
        down_sql: None,
        is_already_applied: None,
    }],
    postgres: &[],
};

#[test]
fn raw_policy_validates_without_filesystem_or_database_effects() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = dir.path().join("not-created");
    let mut policy = DiskGuardEnvironment::default()
        .resolve(Some(0), Some(100))
        .unwrap();
    policy.guard_deadline_ms = 99;
    assert!(MigrationWritePolicy::new(policy, &lock_dir).is_err());
    policy.guard_deadline_ms = 100;
    assert!(MigrationWritePolicy::new(policy, "relative-locks").is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn explicit_policy_runs_all_raw_wrappers_and_refuses_bootstrap_below_floor() {
    let dir = tempfile::tempdir().unwrap();
    let policy = MigrationWritePolicy::new(
        DiskGuardEnvironment::default()
            .resolve(Some(0), Some(100))
            .unwrap(),
        dir.path().join("volume-locks"),
    )
    .unwrap();
    let mut conn = Connection::open(dir.path().join("explicit.db")).unwrap();
    apply_schema_plan_with_policy(&mut conn, &SCHEMA, &policy).unwrap();
    conn.execute("INSERT INTO explicit_policy_marker VALUES (7)", [])
        .unwrap();
    assert_eq!(
        run_migrations_with_policy(&mut conn, &policy).unwrap(),
        latest_schema_version()
    );
    // Fresh migration completes the attachment cutover. Its explicit wrappers
    // must accept an already-completed database under the same namespace.
    let backend = khive_db::StorageBackend::sqlite_with_max_readers_and_policies(
        dir.path().join("explicit.db"),
        Some(0),
        khive_db::WalCeilingPolicy::default(),
        policy.disk_guard_config(),
        policy.volume_lock_dir().to_path_buf(),
    )
    .unwrap();
    let sql = backend.sql();
    let owner = khive_db::stores::blob::acquire_database_gc_owner(sql.as_ref())
        .await
        .unwrap();
    stage_attachment_cutover_with_policy(&mut conn, &policy).unwrap();
    finalize_attachment_cutover_with_policy(&mut conn, &policy).unwrap();
    drop(owner);
    assert_eq!(
        conn.query_row("SELECT id FROM explicit_policy_marker", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        7
    );

    let refusal = MigrationWritePolicy::new(
        DiskGuardEnvironment::default()
            .resolve(Some(u64::MAX), Some(100))
            .unwrap(),
        dir.path().join("volume-locks"),
    )
    .unwrap();
    let mut empty = Connection::open(dir.path().join("refused.db")).unwrap();
    assert!(matches!(
        apply_schema_plan_with_policy(&mut empty, &SCHEMA, &refusal),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let tables: i64 = empty
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tables, 0,
        "raw bootstrap refusal precedes tracking-table DDL"
    );
}

#[test]
fn compatibility_wrapper_requires_explicit_environment_namespace() {
    const CHILD: &str = "KHIVE_RAW_MIGRATION_POLICY_CHILD";
    const MARKER: &str = "RAW_MIGRATION_EXPLICIT_ENV_COMPLETE";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        let mut conn = Connection::open(root.join("compat.db")).unwrap();
        assert!(
            matches!(
                run_migrations(&mut conn),
                Err(SqliteError::CapacityUnavailable {
                    phase: CapacityUnavailablePhase::Lock,
                    ..
                })
            ),
            "missing namespace must refuse without a HOME fallback"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(std::fs::read_dir(root.join("home")).unwrap().count(), 0);
        std::env::set_var("KHIVE_VOLUME_LOCK_DIR", root.join("locks"));
        assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
        let captured = MigrationWritePolicy::from_environment().unwrap();
        std::env::remove_var("KHIVE_VOLUME_LOCK_DIR");
        std::env::set_var("KHIVE_SQLITE_DISK_RESERVE_BYTES", "invalid-after-capture");
        assert_eq!(
            run_migrations_with_policy(&mut conn, &captured).unwrap(),
            latest_schema_version(),
            "explicit policy must not reread mutable environment"
        );
        assert_eq!(std::fs::read_dir(root.join("home")).unwrap().count(), 0);
        println!("{MARKER}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "compatibility_wrapper_requires_explicit_environment_namespace",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, dir.path())
        .env("HOME", &home)
        .env_remove("KHIVE_VOLUME_LOCK_DIR")
        .env_remove("KHIVE_DB_FREE_SPACE_FLOOR_BYTES")
        .env("KHIVE_SQLITE_DISK_RESERVE_BYTES", "0")
        .env("KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS", "100")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert_eq!(
        stdout.matches(MARKER).count(),
        1,
        "must execute the exact child body once: {stdout}"
    );
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored"),
        "nonvacuous exact child result: {stdout}"
    );
}
