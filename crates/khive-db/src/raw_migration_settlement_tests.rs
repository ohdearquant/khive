use super::*;
use crate::disk_guard::{observe_close_with_lease, VolumeIdentity};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

const CREATE: &str = "CREATE TABLE raw_uncommitted (id INTEGER PRIMARY KEY); INSERT INTO raw_uncommitted VALUES (1);";
const BAD_SQL: &str = "CREATE TABLE raw_uncommitted (id INTEGER PRIMARY KEY); INSERT INTO raw_uncommitted VALUES (1); SELECT * FROM missing_raw_migration_table;";

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
            .resolve(Some(0), Some(100))
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
        AuthAction::Transaction {
            operation: TransactionOperation::Commit,
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
        Failure::Statement | Failure::Commit => assert!(result.unwrap().is_err()),
    }
    assert_eq!(closed.load(Ordering::SeqCst), 1, "RAW_MIGRATION_CLOSE_BEFORE_LEASE_RELEASE: denied rollback must close the original SQLite connection while the physical-volume lease is still held");
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
            .resolve(Some(0), Some(100))
            .unwrap(),
        dir.path().join("locks"),
    )
    .unwrap();
    let mut conn = Connection::open(dir.path().join("retained.db")).unwrap();
    conn.execute_batch("CREATE TEMP TABLE original_connection (id INTEGER); INSERT INTO original_connection VALUES (7)").unwrap();
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
    assert!(!conn.is_autocommit(), "an inherited caller transaction must not be committed or rolled back by the rejected migration");
    conn.execute_batch("ROLLBACK").unwrap();
}

#[cfg(unix)]
#[test]
fn raw_unsettled_close_failure_aborts_with_diagnostics_and_releases_volume() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const CHILD: &str = "KHIVE_RAW_SETTLEMENT_ABORT_CHILD";
    const TEST: &str = "migrations::raw_settlement_tests::raw_unsettled_close_failure_aborts_with_diagnostics_and_releases_volume";
    if let Some(root) = std::env::var_os(CHILD) {
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // This child deliberately aborts; a core image would be unbounded fixture output.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) }, 0);
        fn write_before_failure(conn: &Connection) -> bool {
            conn.execute_batch(CREATE).unwrap();
            std::mem::forget(conn.prepare("SELECT 1").unwrap());
            eprintln!("RAW_TERMINAL_ACTUAL_WRITE_DONE");
            false
        }
        let root = PathBuf::from(root);
        let policy = MigrationWritePolicy::new(
            crate::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
            root.join("locks"),
        )
        .unwrap();
        let mut conn = Connection::open(root.join("abort.db")).unwrap();
        conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .unwrap();
        crate::connection_settlement::force_rollback_denial_for_test();
        let plan = ServiceSchemaPlan {
            service: "terminal-raw-settlement",
            sqlite: &[Migration {
                id: "unsettled",
                up_sql: "SELECT * FROM missing_terminal_fixture_table",
                down_sql: None,
                is_already_applied: Some(write_before_failure),
            }],
            postgres: &[],
        };
        let _ = apply_schema_plan_with_policy(&mut conn, &plan, &policy);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir.path())
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "raw terminal child timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    eprintln!("{stderr}");
    assert!(
        stdout.contains("running 1 test"),
        "the child must select the exact fixture: {stdout}"
    );
    assert_eq!(output.status.signal(), Some(libc::SIGABRT), "RAW_TERMINAL_MUST_ABORT: a still-unsettled connection with failed close must not return; status={} stdout={stdout} stderr={stderr}", output.status);
    assert_eq!(stderr.matches("RAW_TERMINAL_ACTUAL_WRITE_DONE").count(), 1);
    assert!(stderr.contains("owned SQLite settlement failed; aborting"));
    assert!(stderr.contains(&format!(
        "path={}",
        std::fs::canonicalize(dir.path().join("abort.db"))
            .unwrap()
            .display()
    )));
    let identity = VolumeIdentity::resolve(&dir.path().join("abort.db")).unwrap();
    assert!(stderr.contains(&format!("volume={}", identity.diagnostic_key())));
    assert!(stderr.contains("rollback_error=not authorized"));
    assert!(stderr.contains(
        "close_error=unable to close due to unfinalized statements or unfinished backups"
    ));
    let _lease = identity
        .acquire(Duration::from_millis(2000), Some(&dir.path().join("locks")))
        .expect("process exit must release the real volume lease");
    let conn = Connection::open(dir.path().join("abort.db")).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let persisted: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='raw_uncommitted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        persisted, 0,
        "aborted process must not persist the uncommitted write"
    );
    conn.execute_batch("ROLLBACK").unwrap();
}
