use super::*;
use crate::disk_guard::{observe_close_with_lease, VolumeIdentity};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

// Every lease in a process shares one in-process slot per volume (ADR-154
// section 3), so a short deadline here loses to unrelated tests holding a write
// on the same volume and fails before any SQL runs. Use the default guard
// deadline that those writers are sized against.
const GUARD_DEADLINE_MS: u64 = 2000;

const CREATE: &str = "CREATE TABLE raw_uncommitted (id INTEGER PRIMARY KEY); \
                      INSERT INTO raw_uncommitted VALUES (1);";
const BAD_SQL: &str = "CREATE TABLE raw_uncommitted (id INTEGER PRIMARY KEY); \
                       INSERT INTO raw_uncommitted VALUES (1); \
                       SELECT * FROM missing_raw_migration_table;";

#[derive(Clone, Copy)]
enum Failure {
    Statement,
    Commit,
    Unwind,
}

fn panic_after_write(conn: &Connection) -> bool {
    conn.execute_batch(CREATE).unwrap();
    panic!("raw migration fixture panic after a write");
}

fn refusal_closes_before_release(failure: Failure) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("first.db");
    let second_path = dir.path().join("second.db");
    let locks = dir.path().join("locks");
    let policy = MigrationWritePolicy::new(
        crate::DiskGuardEnvironment::default()
            .resolve(Some(0), Some(GUARD_DEADLINE_MS))
            .unwrap(),
        &locks,
    )
    .unwrap();
    let mut conn = Connection::open(&path).unwrap();
    let identity = VolumeIdentity::resolve(&path).unwrap();
    assert_eq!(
        identity,
        VolumeIdentity::resolve(&second_path).unwrap(),
        "both databases must share the physical admission domain"
    );
    let closed = observe_close_with_lease(&conn, locks.join(identity.lock_filename()));
    conn.authorizer(Some(move |context: AuthContext<'_>| match context.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        // SQLite authorizes COMMIT and END under the transaction action with the
        // name "COMMIT"; rusqlite has no variant for it and reports Unknown.
        AuthAction::Transaction {
            operation: TransactionOperation::Unknown,
        } if matches!(failure, Failure::Commit) => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .unwrap();
    static BAD_STEP: &[Migration] = &[Migration {
        id: "bad-statement",
        up_sql: BAD_SQL,
        down_sql: None,
        is_already_applied: None,
    }];
    static COMMIT_STEP: &[Migration] = &[Migration {
        id: "denied-commit",
        up_sql: CREATE,
        down_sql: None,
        is_already_applied: None,
    }];
    static PANIC_STEP: &[Migration] = &[Migration {
        id: "panic",
        up_sql: CREATE,
        down_sql: None,
        is_already_applied: Some(panic_after_write),
    }];
    let plan = ServiceSchemaPlan {
        service: "raw-settlement",
        sqlite: match failure {
            Failure::Statement => BAD_STEP,
            Failure::Commit => COMMIT_STEP,
            Failure::Unwind => PANIC_STEP,
        },
        postgres: &[],
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        apply_schema_plan_with_policy(&mut conn, &plan, &policy)
    }));
    match failure {
        Failure::Unwind => assert!(
            result.is_err(),
            "the migration panic must propagate after cleanup"
        ),
        Failure::Statement | Failure::Commit => {
            let error = result.unwrap().expect_err("the migration must fail");
            assert!(
                !matches!(error, SqliteError::CapacityUnavailable { .. }),
                "the failure must come from the migration, not from admission: {error:?}"
            );
        }
    }
    assert_eq!(
        closed.load(Ordering::SeqCst),
        1,
        "close the original connection before releasing its volume lease"
    );
    assert!(
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .is_err(),
        "the retired caller connection must refuse ordinary use"
    );

    let mut second = Connection::open(&second_path).unwrap();
    let successor = ServiceSchemaPlan {
        service: "raw-successor",
        sqlite: COMMIT_STEP,
        postgres: &[],
    };
    apply_schema_plan_with_policy(&mut second, &successor, &policy).expect(
        "a second database on the same volume may write after actual close and lease release",
    );
    assert_eq!(
        second
            .query_row("SELECT id FROM raw_uncommitted", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let first = Connection::open(&path).unwrap();
    first.busy_timeout(std::time::Duration::ZERO).unwrap();
    first
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the retired connection must no longer hold the first database writer lock");
    let persisted: i64 = first
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='raw_uncommitted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        persisted, 0,
        "closing the failed migration must roll back its DDL and payload"
    );
    first.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn raw_schema_statement_error_closes_before_volume_lease_release() {
    refusal_closes_before_release(Failure::Statement);
}

#[test]
fn raw_schema_commit_error_closes_before_volume_lease_release() {
    refusal_closes_before_release(Failure::Commit);
}

#[test]
fn raw_schema_unwind_closes_before_volume_lease_release() {
    refusal_closes_before_release(Failure::Unwind);
}

#[test]
fn raw_schema_success_preserves_connection_and_rejects_inherited_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let policy = MigrationWritePolicy::new(
        crate::DiskGuardEnvironment::default()
            .resolve(Some(0), Some(GUARD_DEADLINE_MS))
            .unwrap(),
        dir.path().join("locks"),
    )
    .unwrap();
    let mut conn = Connection::open(dir.path().join("retained.db")).unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE original_connection (id INTEGER); \
         INSERT INTO original_connection VALUES (7)",
    )
    .unwrap();
    let plan = ServiceSchemaPlan {
        service: "retained",
        sqlite: &[],
        postgres: &[],
    };
    apply_schema_plan_with_policy(&mut conn, &plan, &policy).unwrap();
    assert_eq!(
        conn.query_row("SELECT id FROM temp.original_connection", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        7
    );
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        apply_schema_plan_with_policy(&mut conn, &plan, &policy),
        Err(SqliteError::InheritedWriterTransaction)
    ));
    assert!(
        !conn.is_autocommit(),
        "a rejected migration must preserve the caller's transaction"
    );
    conn.execute_batch("ROLLBACK").unwrap();
}

#[cfg(unix)]
#[test]
fn raw_unsettled_close_failure_returns_unknown_outcome_and_releases_volume() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsettled.db");
    let locks = dir.path().join("locks");
    let policy = MigrationWritePolicy::new(
        crate::DiskGuardEnvironment::default()
            .resolve(Some(0), Some(GUARD_DEADLINE_MS))
            .unwrap(),
        &locks,
    )
    .unwrap();
    let mut conn = Connection::open(&path).unwrap();
    let identity = VolumeIdentity::resolve(&path).unwrap();
    let closed = observe_close_with_lease(&conn, locks.join(identity.lock_filename()));
    conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .unwrap();
    std::mem::forget(conn.prepare("SELECT 1").unwrap());
    crate::connection_settlement::force_rollback_denial_for_test();

    let plan = ServiceSchemaPlan {
        service: "raw-unknown-outcome",
        sqlite: &[Migration {
            id: "unsettled",
            up_sql: "SELECT 1",
            down_sql: None,
            is_already_applied: Some(panic_after_write),
        }],
        postgres: &[],
    };
    assert!(matches!(
        apply_schema_plan_with_policy(&mut conn, &plan, &policy),
        Err(SqliteError::WriterSettlementUnknown)
    ));
    assert_eq!(
        closed.load(Ordering::SeqCst),
        0,
        "the leaked statement keeps the original handle open, so it is never closed"
    );
    let replacement = conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0));
    assert!(
        matches!(
            replacement,
            Err(rusqlite::Error::SqliteFailure(ref error, _))
                if error.code == rusqlite::ErrorCode::AuthorizationForStatementDenied
        ),
        "the caller receives an inert replacement connection that refuses every statement: \
         {replacement:?}"
    );
    drop(conn);

    let _lease = identity
        .acquire(Duration::from_millis(2000), Some(&locks))
        .expect("terminal settlement releases the volume lease");
}
