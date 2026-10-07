//! Exercise guarded code-map admission through public dependency builds.

use khive_db::{DiskGuardEnvironment, SqliteError, StorageBackend};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::test_support::run_exact_test_in_child;

const CHILD: &str = "KHIVE_CODE_MAP_DISK_POLICY_CHILD";
const ROOT: &str = "KHIVE_CODE_MAP_DISK_POLICY_ROOT";

fn private_root() -> tempfile::TempDir {
    let physical_temp = std::env::temp_dir().canonicalize().unwrap();
    tempfile::tempdir_in(physical_temp).unwrap()
}

#[test]
fn code_map_runtime_uses_captured_policy_and_lock_directory() {
    let mut fixture = None;
    if run_exact_test_in_child(CHILD, false, |command| {
        let root = private_root();
        command
            .env_clear()
            .env("HOME", root.path().join("home"))
            .env("KHIVE_TEST_HARNESS", "1")
            .env(ROOT, root.path())
            .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("captured-locks"))
            .env("KHIVE_SQLITE_DISK_RESERVE_BYTES", "0")
            .env("KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS", "173");
        fixture = Some(root);
    }) {
        let root = fixture.unwrap();
        assert!(
            !root.path().join("home").exists(),
            "code-map fixture must not write HOME"
        );
        assert!(!root.path().join("uncaptured-locks").exists());
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
    let path = root.join("captured.db");
    let config = RuntimeConfig {
        db_path: Some(path.clone()),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    };
    let expected = config.disk_guard_environment.resolve(None, None).unwrap();
    assert_eq!(expected.reserve_bytes, 0);
    assert_eq!(expected.guard_deadline_ms, 173);
    // Only this exact-test child mutates its environment after capture.
    std::env::set_var("KHIVE_SQLITE_DISK_RESERVE_BYTES", "invalid-after-capture");
    std::env::set_var(
        "KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS",
        "invalid-after-capture",
    );
    std::env::set_var("KHIVE_VOLUME_LOCK_DIR", root.join("uncaptured-locks"));
    let runtime = KhiveRuntime::new_code_map(config, vec![], vec![]).unwrap();
    let pool = runtime.backend().pool();
    assert_eq!(runtime.config().disk_guard_config, Some(expected));
    assert_eq!(pool.effective_disk_guard_config(), Some(expected));
    assert_eq!(
        pool.config().volume_lock_dir.as_deref(),
        Some(root.join("captured-locks").as_path())
    );
    assert!(
        pool.config().code_map_vfs.is_some(),
        "admission must retain the guarded VFS"
    );
    assert!(!pool.config().wal_mode);
    {
        let writer = pool.try_writer().unwrap();
        assert_eq!(
            writer
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        writer
            .execute_batch(
                "CREATE TABLE captured_policy_write (id INTEGER); \
                 INSERT INTO captured_policy_write VALUES (7)",
            )
            .unwrap();
    }
    drop(runtime);
    let stored =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        stored
            .query_row("SELECT id FROM captured_policy_write", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        7
    );
    drop(stored);
    assert!(root.join("captured-locks").is_dir());
    assert!(!root.join("uncaptured-locks").exists());

    let floor = DiskGuardEnvironment::default()
        .resolve(Some(u64::MAX), Some(100))
        .unwrap();
    let refused_path = root.join("floor.db");
    let refused = StorageBackend::sqlite_code_map_with_policies(
        &refused_path,
        &[],
        &[],
        floor,
        root.join("captured-locks"),
    )
    .unwrap();
    assert!(
        matches!(
            refused.prepare_core_schema(),
            Err(SqliteError::CapacityFloor { .. })
        ),
        "captured code-map policy must gate the core migration hook"
    );
    let reader = refused.pool().reader().unwrap();
    assert_eq!(
        reader
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "floor refusal must precede identity and migration schema writes"
    );
    drop(reader);
    drop(refused);

    let missing = root.join("missing-parent");
    let config = RuntimeConfig {
        db_path: Some(missing.join("map.db")),
        packs: vec![],
        disk_guard_config: Some(expected),
        volume_lock_dir: Some(root.join("captured-locks")),
        ..RuntimeConfig::no_embeddings()
    };
    assert!(KhiveRuntime::new_code_map(config, vec![], vec![]).is_err());
    assert!(
        !missing.exists(),
        "the guarded constructor must not create an explicit parent"
    );
}

#[test]
fn code_map_explicit_policy_validates_before_target_effects() {
    let root = private_root();
    let path = root.path().join("invalid.db");
    let mut policy = DiskGuardEnvironment::default()
        .resolve(Some(0), Some(100))
        .unwrap();
    policy.guard_deadline_ms = 99;
    assert!(matches!(
        StorageBackend::sqlite_code_map_with_policies(
            &path,
            &[],
            &[],
            policy,
            root.path().join("locks"),
        ),
        Err(SqliteError::InvalidConfig(_))
    ));
    policy.guard_deadline_ms = 100;
    assert!(matches!(
        StorageBackend::sqlite_code_map_with_policies(
            &path,
            &[],
            &[],
            policy,
            "relative-locks".into(),
        ),
        Err(SqliteError::InvalidConfig(_))
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
