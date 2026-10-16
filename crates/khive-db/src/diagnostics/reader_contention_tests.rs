#[test]
fn diagnostics_exposes_reader_saturation_and_completed_hold_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A pre-opened reader makes setup independent of the exhaustion timeout.
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("reader_saturation.db")),
        max_readers: 1,
        checkout_timeout: Duration::from_millis(2),
        ..PoolConfig::default()
    })
    .expect("one-reader file-backed pool");
    let held = pool.reader().expect("first reader checkout");
    assert!(
        pool.reader().is_err(),
        "the live checkout must exhaust the one-slot reader budget"
    );
    drop(held);

    let report = collect(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
    );
    let reader = report.reader_contention;
    assert_eq!(reader.reader_admission_capacity, 1);
    assert_eq!(reader.available_reader_admission_slots, 1);
    assert_eq!(reader.reader_acquisitions, 1);
    assert_eq!(reader.pooled_reader_checkouts, 1);
    assert_eq!(reader.standalone_reader_opens, 0);
    assert_eq!(reader.infrastructure_standalone_reader_opens, 0);
    assert_eq!(reader.reader_checkout_timeouts, 1);
    assert_eq!(reader.active_pooled_reader_checkouts, 0);
    assert_eq!(reader.peak_active_pooled_reader_checkouts, 1);
    assert_eq!(reader.completed_pooled_reader_checkouts, 1);
    assert!(reader.max_completed_reader_hold_micros > 0);

    let json = serde_json::to_value(&report).expect("report serializes");
    assert_eq!(
        json.pointer("/reader_contention/reader_admission_capacity"),
        Some(&serde_json::json!(1)),
        "the operator wire payload must expose the reader admission budget"
    );
    assert_eq!(
        json.pointer("/reader_contention/reader_checkout_timeouts"),
        Some(&serde_json::json!(1)),
        "the operator wire payload must expose the reader timeout phase"
    );
    assert!(
        json.pointer("/reader_contention/max_completed_reader_hold_micros")
            .is_some(),
        "the operator wire payload must expose completed hold-time evidence"
    );
}

/// A query refused with SQLITE_BUSY after the busy handler gives up shows up
/// in `reader_busy_timeouts`, apart from `reader_checkout_timeouts`. WAL
/// readers are never blocked by a writer, so the fixture uses a
/// rollback-journal database, where a connection holding an exclusive lock
/// refuses every other reader.
#[test]
fn diagnostics_counts_reader_busy_handler_timeouts_apart_from_checkout_timeouts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("reader_busy_timeouts.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        wal_mode: false,
        write_queue_enabled: Some(false),
        busy_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    })
    .expect("rollback-journal file-backed pool");
    pool.writer()
        .expect("writer")
        .conn()
        .execute_batch("CREATE TABLE busy_fixture (id INTEGER PRIMARY KEY)")
        .expect("fixture table");

    // Control: with no lock held the read succeeds and nothing is counted.
    let reader = pool.reader().expect("reader checkout");
    let rows = reader
        .query_row("SELECT count(*) FROM busy_fixture", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("unlocked read");
    assert_eq!(rows, 0);
    drop(reader);
    assert_eq!(
        ReaderContentionDiagnostics::snapshot(&pool).reader_busy_timeouts,
        0
    );

    let holder = Connection::open(&path).expect("second connection");
    holder
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    let reader = pool.reader().expect("reader checkout");
    let refused = reader
        .query_row("SELECT count(*) FROM busy_fixture", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect_err("a read behind an exclusive lock must be refused");
    assert!(
        matches!(
            &refused,
            crate::SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "the refusal must be SQLITE_BUSY: {refused}"
    );
    holder.execute_batch("ROLLBACK").expect("release lock");
    drop(reader);

    let snapshot = ReaderContentionDiagnostics::snapshot(&pool);
    assert_eq!(snapshot.reader_busy_timeouts, 1);
    assert_eq!(
        snapshot.reader_checkout_timeouts, 0,
        "a busy-handler refusal after checkout is not a checkout timeout"
    );
    let json = serde_json::to_value(snapshot).expect("snapshot serializes");
    assert_eq!(
        json.pointer("/reader_busy_timeouts"),
        Some(&serde_json::json!(1)),
        "the operator wire payload must expose the busy-handler count"
    );
}

#[test]
fn diagnostics_reports_configured_reader_budget_and_both_deadlines() {
    let pool = ConnectionPool::new(PoolConfig {
        max_readers: 6,
        checkout_timeout: Duration::from_millis(17),
        busy_timeout: Duration::from_millis(31),
        ..PoolConfig::default()
    })
    .expect("in-memory pool");
    let report = collect(
        &pool,
        BuildIdentity::from_env("9.9.9", None),
        Duration::from_secs(30),
    );
    let reader = report.reader_contention;
    assert_eq!(reader.reader_admission_capacity, 1);

    let json = serde_json::to_value(&report).expect("report serializes");
    assert_eq!(
        json.pointer("/reader_contention/configured_reader_cap"),
        Some(&serde_json::json!(6))
    );
    assert_eq!(
        json.pointer("/reader_contention/configured_checkout_timeout_ms"),
        Some(&serde_json::json!(17))
    );
    assert_eq!(
        json.pointer("/reader_contention/configured_busy_timeout_ms"),
        Some(&serde_json::json!(31))
    );
}

#[test]
fn diagnostics_reports_reader_discards_after_recycling_without_counting_its_probe() {
    for reader_max_ops in [2, 5000] {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("reader_discards.db")),
            max_readers: 1,
            reader_max_age: Duration::MAX,
            reader_max_ops,
            checkout_timeout: Duration::from_millis(10),
            write_queue_enabled: Some(false),
            disk_guard_config: Some(crate::disk_guard_config::EffectiveDiskGuardConfig {
                reserve_bytes: 0,
                ..Default::default()
            }),
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            ..PoolConfig::for_test()
        })
        .expect("one-reader file-backed pool");

        for completed in 0..=4 {
            if completed > 0 {
                let reader = pool.reader().expect("reader checkout");
                for _ in 0..4 {
                    assert_eq!(
                        reader
                            .query_row("SELECT 7", [], |row| row.get::<_, i64>(0))
                            .expect("reader remains usable"),
                        7
                    );
                }
                drop(reader);
            }
            let expected_discards = u64::from(reader_max_ops == 2 && completed >= 3);
            let before = pool.reader_acquisition_snapshot();
            assert_eq!(before.reader_discards, expected_discards);
            let report = collect(
                &pool,
                BuildIdentity::from_env("9.9.9", None),
                Duration::from_secs(30),
            );
            let reader = report.reader_contention;
            assert_eq!(reader.reader_discards, expected_discards);
            assert_eq!(reader.reader_discards, before.reader_discards);
            assert_eq!(reader.completed_pooled_reader_checkouts, completed);
            assert_eq!(reader.active_pooled_reader_checkouts, 0);
            assert_eq!(reader.available_reader_admission_slots, 1);
            assert_eq!(reader.reader_replacement_open_failures, 0);
            assert_eq!(pool.available_readers(), 1);
            assert_eq!(
                pool.reader_acquisition_snapshot(),
                before,
                "collecting diagnostics must not consume a checkout or recycle a reader"
            );
            let json = serde_json::to_value(&report).expect("report serializes");
            assert_eq!(
                json.pointer("/reader_contention/reader_discards"),
                Some(&serde_json::json!(expected_discards))
            );
        }
    }
}
