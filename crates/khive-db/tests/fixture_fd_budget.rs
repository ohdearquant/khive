#![cfg(unix)]

use khive_db::{ConnectionPool, PoolConfig};
use std::process::{Command, Stdio};

const TEST_NAME: &str = "fixture_databases_fit_default_open_file_limit";
const CHILD_PARENT: &str = "KHIVE_FD_BUDGET_PARENT";
const FIXTURE_ROOT: &str = "KHIVE_FD_BUDGET_ROOT";

#[test]
fn fixture_databases_fit_default_open_file_limit() {
    // Only this exact child changes its limit; other tests keep their normal
    // concurrency and the parent process keeps its original resource limits.
    // SAFETY: getppid takes no pointers and has no preconditions.
    let parent_pid = unsafe { libc::getppid() }.to_string();
    if std::env::var(CHILD_PARENT).as_deref() == Ok(parent_pid.as_str()) {
        hold_fixture_databases();
        return;
    }

    let root = tempfile::tempdir().expect("private fixture directory");
    let home = root.path().join("home");
    let databases = root.path().join("databases");
    std::fs::create_dir(&home).expect("private home");
    std::fs::create_dir(&databases).expect("private database directory");
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("KHIVE_") {
            child.env_remove(name);
        }
    }
    let output = child
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_PARENT, std::process::id().to_string())
        .env(FIXTURE_ROOT, &databases)
        .env("KHIVE_TEST_HARNESS", "1")
        .env("HOME", &home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn bounded fixture child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed;"),
        "FD_BUDGET_FIXTURE_CHILD_FAILED: {stdout}\n{stderr}"
    );
    assert!(
        std::fs::read_dir(&home)
            .expect("private home entries")
            .next()
            .is_none(),
        "fixture pools must not write to HOME"
    );
}

fn hold_fixture_databases() {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limits is a valid writable rlimit for this process.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
        0
    );
    assert!(
        limits.rlim_cur >= 256 && limits.rlim_max >= 256,
        "child needs an available limit of 256; this test never raises it"
    );
    limits.rlim_cur = 256;
    // SAFETY: limits is initialized; the hard limit is unchanged and the soft
    // limit only decreases inside this dedicated child process.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);

    let root =
        std::path::PathBuf::from(std::env::var_os(FIXTURE_ROOT).expect("private database root"));
    let mut pools = Vec::new();
    for index in 0..16 {
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(root.join(format!("fixture-{index}.db"))),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap_or_else(|error| panic!("FD_BUDGET_POOL_OPEN {index}: {error:?}"));
        pool.writer()
            .expect("fixture writer")
            .conn()
            .execute_batch("CREATE TABLE fixture_row (id INTEGER PRIMARY KEY); INSERT INTO fixture_row VALUES (1)")
            .expect("seed real WAL database");
        let readers: Vec<_> = (0..pool.max_readers())
            .map(|_| pool.reader().expect("fixture reader"))
            .collect();
        for reader in &readers {
            let count: i64 = reader
                .query_row("SELECT count(*) FROM fixture_row", [], |row| row.get(0))
                .expect("touch each reader's real WAL database");
            assert_eq!(count, 1);
        }
        drop(readers);
        pools.push(pool);
    }
    assert_eq!(pools.len(), 16);
}
