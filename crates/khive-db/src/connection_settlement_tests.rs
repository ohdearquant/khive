use crate::disk_guard::VolumeIdentity;
use crate::{ConnectionPool, DiskGuardEnvironment, PoolConfig, SqliteError};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use rusqlite::Connection;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn poison(conn: &Connection, terminal: bool) {
    conn.execute_batch("INSERT INTO admission_payload VALUES (1)")
        .unwrap();
    std::mem::forget(conn.prepare("SELECT 1").unwrap());
    conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .unwrap();
    if terminal {
        super::force_rollback_denial_for_test();
    }
    eprintln!("OWNED_SETTLEMENT_ACTUAL_WRITE_AND_STATEMENT_LEAK");
}

fn assert_recovered(root: &Path) {
    let path = root.join("owner.db");
    let identity = VolumeIdentity::resolve(&path).unwrap();
    let _lease = identity
        .acquire(Duration::from_millis(2000), Some(&root.join("locks")))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("OWNED_SETTLEMENT_LIVE_REOPEN: old owner must settle before this process exits");
    assert_eq!(
        conn.query_row("SELECT count(*) FROM admission_payload", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute_batch("ROLLBACK").unwrap();
    let second_path = root.join("same-volume.db");
    assert_eq!(identity, VolumeIdentity::resolve(&second_path).unwrap());
    let second = Connection::open(second_path).unwrap();
    second
        .execute_batch("CREATE TABLE successor (id INTEGER); INSERT INTO successor VALUES (1)")
        .unwrap();
}

fn exercise_owner(root: &Path, owner: &str, terminal: bool) {
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // The terminal child intentionally aborts; do not create unbounded core output.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) }, 0);
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(root.join("owner.db")),
        volume_lock_dir: Some(root.join("locks")),
        disk_guard_config: Some(
            DiskGuardEnvironment::default()
                .resolve(Some(0), Some(100))
                .unwrap(),
        ),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.writer()
        .unwrap()
        .execute_batch(include_str!("../tests/fixtures/disk-admission.sql"))
        .unwrap();
    match owner {
        "pooled" => {
            let writer = pool.writer().unwrap();
            let result: Result<(), SqliteError> = writer.transaction(|conn| {
                poison(conn, terminal);
                Err(SqliteError::InvalidData(
                    "retire leaked-statement pooled writer".into(),
                ))
            });
            assert!(result.is_err());
            drop(writer);
            assert!(pool.try_writer().is_err());
        }
        "standalone" => {
            let unit = pool.standalone_transaction_write_unit().unwrap();
            poison(unit.conn(), terminal);
            drop(unit);
        }
        "lifetime" => {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let writer = crate::writer_task::spawn(&pool, 1).unwrap();
                    let result = writer
                        .send(move |conn| {
                            poison(conn, terminal);
                            Err::<(), _>(khive_storage::StorageError::Internal(
                                "retire leaked-statement lifetime writer".into(),
                            ))
                        })
                        .await;
                    assert!(result.is_err());
                    drop(writer);
                    pool.take_writer_task_join().unwrap().await.unwrap();
                });
        }
        _ => panic!("unknown owned-settlement fixture: {owner}"),
    }
    // This must execute while the deliberately leaked statement is still alive.
    assert_recovered(root);
    eprintln!("OWNED_SETTLEMENT_LIVE_REOPEN_PASSED");
}

fn run_case(owner: &str, terminal: bool) {
    const CHILD: &str = "KHIVE_OWNED_SETTLEMENT_CHILD";
    let thread = std::thread::current();
    let test = thread.name().unwrap();
    if let Some(root) = std::env::var_os(CHILD) {
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            ["--exact", test, "--nocapture", "--test-threads=1"]
        );
        exercise_owner(Path::new(&root), owner, terminal);
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, root.path())
        .current_dir(root.path())
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
                "owned-settlement child timed out: {}",
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
        "exact child fixture must be selected: {stdout}"
    );
    assert_eq!(
        stderr
            .matches("OWNED_SETTLEMENT_ACTUAL_WRITE_AND_STATEMENT_LEAK")
            .count(),
        1
    );
    if terminal {
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "OWNED_SETTLEMENT_MUST_ABORT: {owner} may not release an unsettled owner: status={} stdout={stdout} stderr={stderr}", output.status);
    } else {
        assert!(output.status.success(), "OWNED_SETTLEMENT_LIVE_REOPEN: {owner} must recover before child exit: {stdout} {stderr}");
    }
    let identity = VolumeIdentity::resolve(&root.path().join("owner.db")).unwrap();
    assert!(
        stderr.contains(&format!("volume={}", identity.diagnostic_key())),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "path={}",
            root.path()
                .join("owner.db")
                .canonicalize()
                .unwrap()
                .display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "close_error=unable to close due to unfinalized statements or unfinished backups"
        ),
        "SQLite must report actual close BUSY: {stderr}"
    );
    if terminal {
        assert!(stderr.contains("owned SQLite settlement failed; aborting"));
        assert!(stderr.contains("rollback_error=not authorized"));
        assert!(!stderr.contains("OWNED_SETTLEMENT_LIVE_REOPEN_PASSED"));
        assert_recovered(root.path());
    } else {
        assert!(
            stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored"),
            "{stdout}"
        );
        assert_eq!(
            stderr
                .matches("OWNED_SETTLEMENT_AUTOCOMMIT_RECOVERED")
                .count(),
            1
        );
        assert_eq!(
            stderr
                .matches("OWNED_SETTLEMENT_LIVE_REOPEN_PASSED")
                .count(),
            1
        );
    }
}

#[test]
fn pooled_close_busy_recovers_autocommit_before_release() {
    run_case("pooled", false);
}
#[test]
fn standalone_close_busy_recovers_autocommit_before_release() {
    run_case("standalone", false);
}
#[test]
fn lifetime_close_busy_recovers_autocommit_before_release() {
    run_case("lifetime", false);
}
#[test]
fn pooled_unsettled_close_busy_aborts_before_release() {
    run_case("pooled", true);
}
#[test]
fn standalone_unsettled_close_busy_aborts_before_release() {
    run_case("standalone", true);
}
#[test]
fn lifetime_unsettled_close_busy_aborts_before_release() {
    run_case("lifetime", true);
}
