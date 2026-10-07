//! Disk-reserve admission, lease and settlement tests for the pool.

use super::*;

#[test]
fn file_pool_without_explicit_volume_lock_dir_fails_closed() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("missing-lock-dir.db");
    let error = match ConnectionPool::new(PoolConfig {
        path: Some(database.clone()),
        disk_guard_config: Some(EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            ..Default::default()
        }),
        volume_lock_dir: None,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("pool must refuse a missing lock directory"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, SqliteError::InvalidConfig(message)
            if message.contains("KHIVE_VOLUME_LOCK_DIR")),
        "a missing lock directory is a typed configuration error naming the variable: {error}"
    );
    assert!(!database.exists(), "refusal must precede the SQLite open");
}

#[test]
fn dual_reserve_env_open_child() {
    let Ok(expected) = std::env::var("KHIVE_DISK_GUARD_TEST_EXPECT") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let opened = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("dual-reserve-env.db")),
        ..PoolConfig::for_test()
    });
    match expected.as_str() {
        "conflict" => assert!(matches!(opened, Err(SqliteError::InvalidConfig(_)))),
        "equal" => assert!(opened.is_ok()),
        other => panic!("unknown disk-guard child expectation {other}"),
    }
}

#[test]
fn both_reserve_env_values_are_checked_at_pool_open() {
    use std::process::Command;

    for (new_value, legacy_value, expected) in [("23", "24", "conflict"), ("23", "23", "equal")] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pool::tests::admission::dual_reserve_env_open_child",
                "--nocapture",
            ])
            .env("KHIVE_SQLITE_DISK_RESERVE_BYTES", new_value)
            .env("KHIVE_DB_FREE_SPACE_FLOOR_BYTES", legacy_value)
            .env_remove("KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS")
            .env("KHIVE_DISK_GUARD_TEST_EXPECT", expected)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child {expected} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test did not run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn typed_pooled_transaction_probes_after_begin_and_recovers_after_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("typed-transaction-capacity.db");
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        let competing = Connection::open(&path).unwrap();
        competing.busy_timeout(Duration::ZERO).unwrap();
        let error = competing
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("the typed unit must sample after SQLite writer acquisition");
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        ));
        Ok(if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            100
        } else {
            101
        })
    });

    assert!(matches!(
        pool.transaction_write_unit(),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let unit = pool
        .transaction_write_unit()
        .expect("a refused unit must leave the pooled writer retryable");
    unit.conn().execute_batch("COMMIT").unwrap();
    assert!(unit.conn().is_autocommit());
    assert_eq!(samples.load(Ordering::SeqCst), 2);
}

#[test]
fn typed_standalone_transaction_probes_after_begin_and_recovers_after_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone-transaction-capacity.db");
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        let competing = Connection::open(&path).unwrap();
        competing.busy_timeout(Duration::ZERO).unwrap();
        let error = competing
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("standalone admission must sample after BEGIN");
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        ));
        Ok(if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            100
        } else {
            101
        })
    });

    assert!(matches!(
        pool.execute_direct_transaction(
            StorageCapability::Sql,
            "standalone_test",
            |_| -> Result<(), StorageError> { panic!("refused standalone body must not run") }
        ),
        Err(StorageError::CapacityFloor { .. })
    ));
    pool.execute_direct_transaction(StorageCapability::Sql, "standalone_test", |conn| {
        conn.execute_batch("CREATE TABLE admitted(id INTEGER PRIMARY KEY)")
            .map_err(|error| StorageError::driver(StorageCapability::Sql, "standalone_test", error))
    })
    .expect("a refused standalone unit must be retryable");
    assert_eq!(samples.load(Ordering::SeqCst), 2);
}

#[test]
fn checkpoint_ownership_claim_bypasses_disk_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("checkpoint-bypass-capacity.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&probes);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    });

    pool.claim_checkpoint_ownership()
        .expect("checkpoint ownership remains available below the reserve");
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    let writer = pool.writer_for_checkpoint_operation().unwrap();
    let autocheckpoint: i64 = writer
        .conn()
        .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))
        .unwrap();
    assert_eq!(autocheckpoint, 0);
}

#[test]
fn zero_reserve_disables_refusal_but_still_samples_the_volume() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("zero-reserve.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let samples = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(0, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    });
    pool.writer()
        .expect("zero reserve writer checkout")
        .transaction(|_| Ok(()))
        .expect("zero reserve does not refuse zero free bytes");
    assert_eq!(samples.load(Ordering::SeqCst), 2);
}

#[test]
fn copy_headroom_checked_add_overflow_refuses_without_wrapping() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("overflow-headroom.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(u64::MAX, |_| Ok(u64::MAX));
    assert!(matches!(
        pool.write_admission().check_with_headroom(1),
        Err(SqliteError::CapacityFloor {
            floor_bytes: u64::MAX,
            required_headroom_bytes: 1,
            ..
        })
    ));
}

#[test]
fn poisoned_pooled_connection_closes_before_its_volume_lease_is_released() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("poison-close.db");
    let locks = fixture.path().join("locks");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        volume_lock_dir: Some(locks.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.writer()
        .unwrap()
        .execute_batch(include_str!("../tests/fixtures/disk-admission.sql"))
        .unwrap();
    let identity = VolumeIdentity::resolve(&path).unwrap();
    let guard = pool.writer().unwrap();
    let closed = crate::disk_guard::observe_close_with_lease(
        guard.conn(),
        locks.join(identity.lock_filename()),
    );
    let result: Result<(), SqliteError> = guard.transaction(|conn| {
        conn.execute_batch("INSERT INTO admission_payload VALUES (1)")?;
        conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }))?;
        Err(SqliteError::InvalidData("force rollback denial".into()))
    });
    assert!(result.is_err());
    assert_eq!(closed.load(Ordering::SeqCst), 0);
    drop(guard);
    assert_eq!(
        closed.load(Ordering::SeqCst),
        1,
        "SQLite must destroy its closure while the original OS lease is still locked"
    );
    assert!(
        pool.try_writer().is_err(),
        "retired pool cannot reuse its inert replacement"
    );
    assert!(pool.probe_retired_pooled_writer_for_test().is_err());
    let _competitor = identity
        .acquire(Duration::from_millis(2000), Some(&locks))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("same-volume successor sees a closed old transaction");
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM admission_payload", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        rows, 0,
        "closing the poisoned connection rolls its uncommitted write back"
    );
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn captured_disk_policy_deadline_is_validated_before_open() {
    let fixture = tempfile::tempdir().unwrap();
    for deadline in [0, 99, 10_001, u64::MAX] {
        let path = fixture.path().join(format!("invalid-{deadline}.db"));
        let result = ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            disk_guard_config: Some(EffectiveDiskGuardConfig {
                guard_deadline_ms: deadline,
                ..Default::default()
            }),
            ..PoolConfig::for_test()
        });
        assert!(matches!(result, Err(SqliteError::InvalidConfig(_))));
        assert!(!path.exists());
    }
}

#[test]
fn native_sqlite_full_survives_guarded_transaction_classification() {
    let fixture = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(fixture.path().join("native-full.db")),
        write_queue_enabled: Some(false),
        disk_guard_config: Some(EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            ..Default::default()
        }),
        ..PoolConfig::for_test()
    })
    .unwrap();
    {
        let writer = pool.writer().unwrap();
        writer
            .execute_batch(include_str!("../tests/fixtures/disk-admission.sql"))
            .unwrap();
        let pages: i64 = writer
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        writer
            .pragma_update(None, "max_page_count", pages + 1)
            .unwrap();
    }
    // The file-backed direct helper opens a new connection; use the pooled unit
    // so SQLite's per-connection page limit is the one exercised by the insert.
    let unit = pool.transaction_write_unit().unwrap();
    let mut raw = None;
    let (result, terminal) = execute_wrapped_transaction(unit.conn(), "native_full", |conn| {
        let outcome = conn.execute(
            "INSERT INTO admission_payload VALUES (zeroblob(2097152))",
            [],
        );
        raw = Some((
            outcome
                .as_ref()
                .err()
                .and_then(rusqlite::Error::sqlite_error_code),
            conn.is_autocommit(),
        ));
        outcome
            .map(|_| ())
            .map_err(|error| StorageError::driver(StorageCapability::Sql, "native_full", error))
    });
    assert_eq!(
        raw,
        Some((Some(rusqlite::ErrorCode::DiskFull), true)),
        "the body must hit a native FULL that SQLite rolled back itself"
    );
    // The wrapper's ROLLBACK then finds no transaction, so the outcome is the
    // terminal unknown that tests/pr3423_sqlite_full.rs pins; it is never
    // rewritten as a guard refusal.
    let error = result.expect_err("SQLite page limit must produce a native FULL");
    assert!(
        matches!(
            error,
            StorageError::WriterTaskTerminated {
                request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
            }
        ),
        "native FULL classification: {error:?}"
    );
    assert_eq!(
        terminal,
        Some(khive_storage::WriterTaskRequestState::SideEffectsUnknown)
    );
    assert!(unit.conn().is_autocommit());
}

#[test]
fn db_capacity_floor_refuses_pooled_transaction_after_begin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capacity.db");
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(100, move |_| {
        let competing = Connection::open(&path).unwrap();
        competing.busy_timeout(Duration::ZERO).unwrap();
        let error = competing
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("the admission sample must run after SQLite writer acquisition");
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(code, _)
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        ));
        Ok(100)
    });

    let writer = pool
        .writer_for_admitted_operation()
        .expect("the lease precedes writer checkout");
    let error = writer
        .transaction::<_, ()>(|_| panic!("the refused transaction body must not run"))
        .expect_err("the reserve must refuse after BEGIN");
    assert!(writer.is_autocommit(), "refusal must finish rollback");
    let mapped = error.into_storage_error(StorageCapability::Sql, "test_write");
    assert!(
        matches!(
            mapped,
            StorageError::CapacityFloor {
                capability: StorageCapability::Sql,
                available_bytes: 100,
                floor_bytes: 100,
                ..
            }
        ),
        "the refusal must keep its typed capacity classification"
    );
    assert_eq!(pool.writer_acquisition_snapshot().pooled_acquisitions, 1);
}

#[test]
fn db_capacity_floor_samples_each_pooled_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("fresh-capacity.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        match observed.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(102),
            1 => Ok(100),
            extra => panic!("unexpected capacity sample {extra}"),
        }
    });

    pool.writer_for_admitted_operation()
        .expect("first writer checkout")
        .transaction(|_| Ok(()))
        .expect("first transaction clears the reserve");
    let second = pool
        .writer_for_admitted_operation()
        .expect("second writer checkout")
        .transaction::<_, ()>(|_| panic!("second body must not run"));
    assert!(
        matches!(
            second,
            Err(SqliteError::CapacityFloor {
                available_bytes: 100,
                ..
            })
        ),
        "the second transaction must see the lower free-space sample"
    );
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    assert_eq!(pool.writer_acquisition_snapshot().pooled_acquisitions, 2);
}

#[test]
fn db_capacity_floor_covers_standalone_and_cancellable_writer_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("other-capacity.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(100, |_| Ok(99));

    assert!(matches!(
        pool.open_standalone_writer(),
        Err(SqliteError::CapacityFloor { .. })
    ));
    assert!(matches!(
        pool.writer_until(|| false),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let counters = pool.writer_acquisition_snapshot();
    assert_eq!(counters.standalone_acquisitions, 0);
    assert_eq!(counters.pooled_acquisitions, 0);
}

#[test]
fn explicit_zero_reserve_warns_at_real_pool_startup_only() {
    use tracing_subscriber::layer::SubscriberExt;
    #[derive(Clone)]
    struct Capture(Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            #[derive(Default)]
            struct Message(String);
            impl tracing::field::Visit for Message {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "message" {
                        self.0 = value.to_owned();
                    }
                }
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut message = Message::default();
            event.record(&mut message);
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), message.0));
        }
    }
    let home = tempfile::tempdir().unwrap();
    if crate::test_process::run_in_child(|command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", home.path().join("sink"))
            .env("KHIVE_WALPIN_SIDECAR", "1")
            .env("KHIVE_WALPIN_CENSUS_BUDGET_MS", "0");
    }) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let warning =
        "SQLite disk reserve is explicitly zero; new logical writes will not be floor-refused";
    for (index, reserve, file_backed) in [(0, 0, true), (1, 1, true), (2, 0, false)] {
        let capture = Capture(Arc::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let policy = crate::disk_guard_config::DiskGuardEnvironment::default()
            .resolve(Some(reserve), Some(100))
            .unwrap();
        let pool = tracing::subscriber::with_default(subscriber, || {
            ConnectionPool::new(PoolConfig {
                path: file_backed.then(|| root.join(format!("warning-{index}.db"))),
                volume_lock_dir: Some(root.join(format!("locks-{index}"))),
                write_queue_enabled: Some(false),
                disk_guard_config: Some(policy),
                ..PoolConfig::for_test()
            })
        })
        .expect("real pool startup");
        assert_eq!(pool.canonical_path().is_some(), file_backed);
        assert_eq!(
            pool.effective_disk_guard_config()
                .map(|policy| policy.reserve_bytes),
            file_backed.then_some(reserve)
        );
        let events = capture.0.lock().unwrap();
        let warnings: Vec<_> = events
            .iter()
            .filter(|(_, message)| message == warning)
            .collect();
        assert_eq!(
            warnings.len(),
            usize::from(file_backed && reserve == 0),
            "captured startup events: {events:?}"
        );
        for (level, _) in warnings {
            assert_eq!(*level, tracing::Level::WARN);
        }
    }
}
