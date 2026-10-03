#![cfg(any(unix, windows))]

use super::*;

struct StartupProbeReset;

impl Drop for StartupProbeReset {
    fn drop(&mut self) {
        STARTUP_SPACE_PROBE.with(|probe| *probe.borrow_mut() = None);
    }
}

fn startup_space(available_bytes: u64) -> StartupProbeReset {
    STARTUP_SPACE_PROBE.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some((100, Arc::new(move |_| Ok(available_bytes))));
    });
    StartupProbeReset
}

fn config(path: &Path) -> PoolConfig {
    PoolConfig {
        path: Some(path.to_path_buf()),
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    }
}

#[test]
fn low_space_identity_refuses_owner_then_remains_stable_after_installation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE existing_data (value INTEGER)")
        .unwrap();
    drop(conn);
    let probe = startup_space(100);
    let recovery = ConnectionPool::new(config(&path)).unwrap();
    let table_count: i64 = recovery
        .reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM main.sqlite_master WHERE name = ?1",
            [DATABASE_ID_TABLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        table_count, 0,
        "low-space open must skip identity installation"
    );
    assert_eq!(
        recovery.database_owner_identity(),
        Err(DatabaseOwnerIdentityError::DurableIdentityUnavailable)
    );
    drop(recovery);
    drop(probe);

    let probe = startup_space(101);
    let installed = ConnectionPool::new(config(&path)).unwrap();
    let owner = installed.database_owner_identity().unwrap();
    let stored: String = installed
        .reader()
        .unwrap()
        .query_row(
            "SELECT id FROM _khive_database_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner.durable_id(), uuid::Uuid::parse_str(&stored).unwrap());
    installed.verify_database_owner(&owner).unwrap();
    drop(installed);
    drop(probe);

    let probe = startup_space(100);
    let recovery = ConnectionPool::new(config(&path)).unwrap();
    assert_eq!(recovery.database_owner_identity().unwrap(), owner);
    recovery.verify_database_owner(&owner).unwrap();
    drop(recovery);
    drop(probe);

    let _probe = startup_space(101);
    let reopened = ConnectionPool::new(config(&path)).unwrap();
    assert_eq!(reopened.database_owner_identity().unwrap(), owner);
    reopened.verify_database_owner(&owner).unwrap();
}

#[test]
fn legacy_read_only_owner_requires_reopen_after_identity_installation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE existing_data (value INTEGER)")
        .unwrap();
    drop(conn);
    let mut read_only_config = config(&path);
    read_only_config.read_only = true;
    let read_only = ConnectionPool::new(read_only_config.clone()).unwrap();
    assert_eq!(
        read_only.database_owner_identity(),
        Err(DatabaseOwnerIdentityError::DurableIdentityUnavailable)
    );

    let _probe = startup_space(101);
    let writable = ConnectionPool::new(config(&path)).unwrap();
    let owner = writable.database_owner_identity().unwrap();
    assert_eq!(
        read_only.database_owner_identity(),
        Err(DatabaseOwnerIdentityError::DurableIdentityUnavailable),
        "the accessor must not refresh or invent missing open-time evidence"
    );
    read_only.open_reader_connection().unwrap();
    drop(read_only);
    drop(writable);
    let reopened = ConnectionPool::new(read_only_config).unwrap();
    assert_eq!(reopened.database_owner_identity().unwrap(), owner);
    reopened.verify_database_owner(&owner).unwrap();
}
