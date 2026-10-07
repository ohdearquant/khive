use super::*;
use crate::{ConnectionPool, PoolConfig};
use std::path::PathBuf;

#[test]
fn apply_schema_plan_rolls_back_migration_when_ledger_insert_fails() {
    static MIGRATIONS: &[Migration] = &[Migration {
        id: "001_atomic",
        up_sql: "CREATE TABLE migration_effect (id INTEGER PRIMARY KEY);",
        down_sql: None,
        is_already_applied: None,
    }];
    let plan = ServiceSchemaPlan {
        service: "atomicity_test",
        sqlite: MIGRATIONS,
        postgres: &[],
    };
    let mut conn = open_memory();
    conn.execute_batch(SCHEMA_VERSION_TABLE).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_schema_version
         BEFORE INSERT ON _schema_versions
         BEGIN
             SELECT RAISE(ABORT, 'injected ledger failure');
         END;",
    )
    .unwrap();

    apply_schema_plan(&mut conn, &plan).expect_err("ledger failure must abort the migration");

    assert!(
        !table_exists(&conn, "migration_effect"),
        "migration body must roll back when its ledger insert fails"
    );
    let ledger_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _schema_versions WHERE service = 'atomicity_test'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_rows, 0);
}

#[test]
fn service_schema_bootstrap_refusal_precedes_its_tracking_table() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("service-bootstrap.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(100, |_| Ok(100));
    let admission = pool.write_admission();
    let plan = ServiceSchemaPlan {
        service: "capacity-bootstrap",
        sqlite: &[],
        postgres: &[],
    };

    assert!(!table_exists(
        pool.writer_for_admitted_operation().unwrap().conn(),
        "_schema_versions"
    ));
    assert!(matches!(
        apply_schema_plan_with_admission(&mut pool.migration_transactions(), &plan, &admission),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let writer = pool.writer_for_admitted_operation().unwrap();
    assert!(!table_exists(writer.conn(), "_schema_versions"));
    assert!(writer.conn().is_autocommit());
}

#[test]
fn core_bootstrap_refusal_precedes_ledger_and_post_begin_refusal_rolls_back() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("core-bootstrap.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(100, |_| Ok(100));
    let admission = pool.write_admission();
    assert!(matches!(
        run_versioned_migrations(&mut pool.migration_transactions(), None, &admission),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let writer = pool.writer_for_admitted_operation().unwrap();
    assert!(!table_exists(writer.conn(), "_schema_migrations"));
    assert!(writer.conn().is_autocommit());
    drop(writer);

    let samples = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        Ok(if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            101
        } else {
            100
        })
    });
    let admission = pool.write_admission();
    assert!(matches!(
        run_versioned_migrations(&mut pool.migration_transactions(), None, &admission),
        Err(SqliteError::CapacityFloor { .. })
    ));
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    let writer = pool.writer_for_admitted_operation().unwrap();
    assert!(table_exists(writer.conn(), "_schema_migrations"));
    assert_eq!(read_schema_version(writer.conn()).unwrap(), 0);
    assert!(writer.conn().is_autocommit());
}

#[test]
fn both_bootstrap_ledgers_refuse_capacity_and_probe_errors_without_wal_growth() {
    for probe_error in [false, true] {
        for service in [true, false] {
            let fixture = tempfile::tempdir().unwrap();
            let path = fixture.path().join("bootstrap-probe.db");
            let mut pool = ConnectionPool::new(PoolConfig {
                path: Some(path.clone()),
                write_queue_enabled: Some(false),
                ..PoolConfig::for_test()
            })
            .unwrap();
            pool.set_test_write_admission(100, move |_| {
                if probe_error {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected probe refusal",
                    ))
                } else {
                    Ok(100)
                }
            });
            let wal = PathBuf::from(format!("{}-wal", path.display()));
            let before = std::fs::metadata(&wal).ok().map(|metadata| metadata.len());
            let admission = pool.write_admission();
            let mut writes = pool.migration_transactions();
            let result = if service {
                apply_schema_plan_with_admission(
                    &mut writes,
                    &ServiceSchemaPlan {
                        service: "probe-refusal",
                        sqlite: &[],
                        postgres: &[],
                    },
                    &admission,
                )
            } else {
                run_versioned_migrations(&mut writes, None, &admission).map(|_| ())
            };
            let writer = pool.writer_for_admitted_operation().unwrap();
            if probe_error {
                assert!(matches!(
                    result,
                    Err(SqliteError::CapacityUnavailable {
                        phase: khive_storage::CapacityUnavailablePhase::Probe,
                        ..
                    })
                ));
            } else {
                assert!(matches!(result, Err(SqliteError::CapacityFloor { .. })));
            }
            assert!(!table_exists(writer.conn(), "_schema_versions"));
            assert!(!table_exists(writer.conn(), "_schema_migrations"));
            assert!(writer.conn().is_autocommit());
            assert_eq!(
                std::fs::metadata(&wal).ok().map(|metadata| metadata.len()),
                before
            );
        }
    }
}

#[test]
fn service_bootstrap_and_migration_have_separate_admission_samples() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    static STEPS: &[Migration] = &[Migration {
        id: "capacity_probe",
        up_sql: "SELECT 1;",
        down_sql: None,
        is_already_applied: None,
    }];
    let fixture = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(fixture.path().join("bootstrap-above.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let samples = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(101)
    });
    let admission = pool.write_admission();
    apply_schema_plan_with_admission(
        &mut pool.migration_transactions(),
        &ServiceSchemaPlan {
            service: "above-reserve",
            sqlite: STEPS,
            postgres: &[],
        },
        &admission,
    )
    .unwrap();
    let writer = pool.writer_for_admitted_operation().unwrap();
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    assert!(table_exists(writer.conn(), "_schema_versions"));
    assert!(writer.conn().is_autocommit());
}
