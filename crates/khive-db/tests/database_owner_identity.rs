use khive_db::{DatabaseOwnerIdentityError, StorageBackend};

#[cfg(any(unix, windows))]
#[test]
fn durable_database_owner_identity_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let first = StorageBackend::sqlite_for_test(&path).unwrap();
    let owner = first.database_owner_identity().unwrap();
    let stored: String = first
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT id FROM _khive_database_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner.durable_id(), uuid::Uuid::parse_str(&stored).unwrap());
    assert_eq!(first.database_owner_identity().unwrap(), owner);
    first.verify_database_owner(&owner).unwrap();
    drop(first);

    let reopened = StorageBackend::sqlite_for_test(&path).unwrap();
    assert_eq!(reopened.database_owner_identity().unwrap(), owner);
    reopened.verify_database_owner(&owner).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn durable_database_owner_identity_survives_service_migrations() {
    static STEPS: &[khive_db::Migration] = &[khive_db::Migration {
        id: "001_owner_fixture",
        up_sql: "CREATE TABLE IF NOT EXISTS owner_fixture_service (value INTEGER NOT NULL);",
        down_sql: None,
        is_already_applied: None,
    }];
    let plan = khive_db::ServiceSchemaPlan {
        service: "owner_fixture_service",
        sqlite: STEPS,
        postgres: &[],
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    let owner = backend.database_owner_identity().unwrap();
    backend.apply_schema(&plan).unwrap();
    backend.apply_schema(&plan).unwrap();
    let applied: i64 = backend
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM _schema_versions WHERE service = ?1",
            [plan.service],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(applied, 1, "the service migration must actually run once");
    assert_eq!(backend.database_owner_identity().unwrap(), owner);
    drop(backend);
    let reopened = StorageBackend::sqlite_for_test(path).unwrap();
    assert_eq!(reopened.database_owner_identity().unwrap(), owner);
    reopened.verify_database_owner(&owner).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn copied_database_is_refused_as_original_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let copy_path = dir.path().join("copy.db");
    let original = StorageBackend::sqlite_for_test(&path).unwrap();
    let owner = original.database_owner_identity().unwrap();
    original.verify_database_owner(&owner).unwrap();
    drop(original);
    std::fs::copy(&path, &copy_path).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(&copy_path).unwrap()
    );

    let original = StorageBackend::sqlite_for_test(path).unwrap();
    let copied = StorageBackend::sqlite_for_test(copy_path).unwrap();
    let copied_owner = copied.database_owner_identity().unwrap();
    assert_eq!(copied_owner.durable_id(), owner.durable_id());
    assert_ne!(copied_owner.file_identity(), owner.file_identity());
    original.verify_database_owner(&owner).unwrap();
    assert_eq!(
        copied.verify_database_owner(&owner),
        Err(DatabaseOwnerIdentityError::OwnerMismatch),
        "copied rows must not convey ownership of the original database"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn secondary_database_is_refused_as_main_owner() {
    let dir = tempfile::tempdir().unwrap();
    let main = StorageBackend::sqlite_for_test(dir.path().join("main.db")).unwrap();
    let secondary = StorageBackend::sqlite_for_test(dir.path().join("secondary.db")).unwrap();
    let main_owner = main.database_owner_identity().unwrap();
    let secondary_owner = secondary.database_owner_identity().unwrap();
    assert_ne!(secondary_owner.durable_id(), main_owner.durable_id());
    assert_ne!(secondary_owner.file_identity(), main_owner.file_identity());
    main.verify_database_owner(&main_owner).unwrap();
    secondary.verify_database_owner(&secondary_owner).unwrap();
    assert_eq!(
        secondary.verify_database_owner(&main_owner),
        Err(DatabaseOwnerIdentityError::OwnerMismatch)
    );
}

#[cfg(any(unix, windows))]
#[test]
fn read_only_database_owner_identity_matches_writable_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let writable = StorageBackend::sqlite_for_test(&path).unwrap();
    let owner = writable.database_owner_identity().unwrap();
    {
        let checkpoint = writable.pool().try_checkpoint_nowait().unwrap();
        assert_eq!(
            checkpoint.truncate().unwrap().busy,
            0,
            "the durable identity must reach the main file before inspection"
        );
    }
    assert!(
        !writable.pool().writer_task_join_was_stored(),
        "this synchronous fixture must not leave an asynchronous writer to close"
    );
    drop(writable);
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let main_before = std::fs::read(&path).unwrap();

    let read_only = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let stored: String = read_only
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT id FROM _khive_database_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(uuid::Uuid::parse_str(&stored).unwrap(), owner.durable_id());
    assert_eq!(read_only.database_owner_identity().unwrap(), owner);
    read_only.verify_database_owner(&owner).unwrap();
    drop(read_only);
    assert_eq!(std::fs::read(&path).unwrap(), main_before);
}

#[test]
fn in_memory_database_has_no_durable_owner() {
    let backend = StorageBackend::memory().unwrap();
    assert_eq!(
        backend.database_owner_identity(),
        Err(DatabaseOwnerIdentityError::InMemory)
    );
}

#[test]
fn malformed_stored_database_identity_refuses_pool_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE _khive_database_identity (singleton INTEGER PRIMARY KEY, id TEXT NOT NULL); \
         INSERT INTO _khive_database_identity VALUES (1, 'not-a-uuid');",
    )
    .unwrap();
    drop(conn);
    let error = match StorageBackend::sqlite_for_test(path) {
        Ok(_) => panic!("a corrupt stored identity must refuse pool construction"),
        Err(error) => error,
    };
    assert!(matches!(&error, khive_db::SqliteError::InvalidData(_)));
    assert!(error
        .to_string()
        .contains("invalid stored database identity"));
}
