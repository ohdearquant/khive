use super::*;

#[test]
fn live_readonly_backend_reads_wal_and_refuses_writes_while_snapshot_stays_frozen() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let path = directory.path().join("live-readonly.db");
    let writable = StorageBackend::sqlite_for_test(&path).expect("writable backend");
    writable.prepare_core_schema().expect("prepare schema");

    let holder = rusqlite::Connection::open(&path).expect("live WAL writer");
    holder
        .pragma_update(None, "wal_autocheckpoint", 0_i64)
        .expect("retain committed WAL frames");
    holder
        .execute_batch(
            "CREATE TABLE live_readonly_probe (value INTEGER NOT NULL); \
             INSERT INTO live_readonly_probe VALUES (1);",
        )
        .expect("commit probe into WAL");
    let shm = directory.path().join("live-readonly.db-shm");
    let wal = directory.path().join("live-readonly.db-wal");
    assert!(
        shm.is_file(),
        "the live writer must retain its shared memory"
    );
    assert!(
        std::fs::metadata(&wal).expect("WAL metadata").len() > 32,
        "the fixture must retain committed WAL frames"
    );

    let frozen_error = match StorageBackend::sqlite_read_only_for_test(&path) {
        Err(error) => error,
        Ok(_) => panic!("the frozen snapshot constructor must refuse live shared memory"),
    };
    assert!(matches!(frozen_error, SqliteError::InvalidData(message)
            if message.contains("writable WAL shared-memory sidecar")));

    let readonly = StorageBackend::sqlite_read_only_live_with_max_readers_and_wal_ceiling(
        &path,
        Some(2),
        WalCeilingPolicy::default(),
    )
    .expect("live read-only backend");
    assert!(readonly.is_read_only());
    assert_eq!(readonly.pool().config().write_queue_enabled, Some(false));
    readonly
        .prepare_core_schema()
        .expect("read-only current-schema validation");
    {
        let reader = readonly.pool().reader().expect("live reader");
        let value: i64 = reader
            .conn()
            .query_row("SELECT value FROM live_readonly_probe", [], |row| {
                row.get(0)
            })
            .expect("committed WAL row");
        assert_eq!(value, 1);
    }
    {
        let writer = readonly.pool().writer().expect("read-only writer slot");
        let query_only: i64 = writer
            .conn()
            .pragma_query_value(None, "query_only", |row| row.get(0))
            .expect("writer-slot read-only policy");
        assert_eq!(query_only, 1);
        let error = writer
            .conn()
            .execute("INSERT INTO live_readonly_probe VALUES (2)", [])
            .expect_err("application writes must be refused");
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly)
        );
        writer
            .conn()
            .pragma_update(None, "query_only", "OFF")
            .expect("disable only the connection-local write gate");
        let native_error = writer
            .conn()
            .execute("INSERT INTO live_readonly_probe VALUES (3)", [])
            .expect_err("native read-only open flags must independently refuse writes");
        assert_eq!(
            native_error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly)
        );
        writer
            .conn()
            .pragma_update(None, "query_only", "ON")
            .expect("restore the writer-slot baseline");
    }
    let count: i64 = holder
        .query_row("SELECT COUNT(*) FROM live_readonly_probe", [], |row| {
            row.get(0)
        })
        .expect("probe count after refusal");
    assert_eq!(count, 1);
    assert!(
        shm.is_file(),
        "read-only planning must retain live shared memory"
    );
}
