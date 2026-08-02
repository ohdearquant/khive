//! Integration coverage for the writer-timeout NDJSON sink's real wiring
//! (`crate::timeout_sink`, wired into `ConnectionPool::writer()` and
//! `SqlBridge`'s standalone-writer path).
//!
//! The sink is a process-global `OnceLock` by design (one file, one
//! heartbeat thread, per process — see the module's doc comment). That
//! makes it a poor fit for `khive-db`'s main unit-test binary, where
//! hundreds of unrelated tests boot pools of their own: whichever pool
//! boots first anywhere in that process wins the sink's log directory, and
//! many of those pools' directories are `tempfile::tempdir()`s that get
//! deleted the moment their own test function returns. This file is a
//! dedicated integration-test binary (its own process, per Cargo's `tests/`
//! convention) containing ONLY sink-wiring tests, so the two tests below are
//! the only code in this process that ever boots a `ConnectionPool` — no
//! other test can race them for the sink's directory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once, OnceLock};
use std::time::Duration;

use khive_db::{ConnectionPool, PoolConfig, SqlBridge};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlAccess;

static SINK_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
static SET_ENV: Once = Once::new();

/// Point the sink at a directory this process controls and keeps alive for
/// its whole lifetime, instead of letting it resolve against whichever
/// test's own (eventually-dropped) tempdir happens to boot the first pool.
/// Idempotent and safe to call at the top of every test in this file
/// regardless of run order — `Once` guarantees the env var is set exactly
/// once, and every test after that agrees on the same directory.
fn ensure_sink_dir() -> &'static Path {
    let dir = SINK_DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"));
    SET_ENV.call_once(|| {
        std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", dir.path());
    });
    dir.path()
}

fn read_sink_ndjson() -> String {
    let path = ensure_sink_dir().join("writer_timeouts.ndjson");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("expected the sink's NDJSON file to exist and be readable at {path:?}: {e}")
    })
}

/// A genuine `ConnectionPool::writer()` admission timeout (a held writer +
/// a tiny `checkout_timeout`) must produce a `"kind":"timeout"` /
/// `"site":"pool_admission"` row naming this pool's own database path.
#[test]
fn writer_admission_timeout_emits_ndjson_row() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("writer_timeout_sink_test.db");
    let cfg = PoolConfig {
        path: Some(db_path.clone()),
        checkout_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(cfg).expect("file-backed pool should open"));

    let held = pool.writer().expect("first checkout should succeed");
    let pool_for_thread = Arc::clone(&pool);
    let timed_out = std::thread::spawn(move || pool_for_thread.writer().is_err())
        .join()
        .unwrap();
    assert!(
        timed_out,
        "a second writer checkout while the first is held must time out"
    );
    drop(held);

    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let contents = read_sink_ndjson();
    let matched = contents.lines().any(|line| {
        line.contains("\"kind\":\"timeout\"")
            && line.contains("\"site\":\"pool_admission\"")
            && line.contains(&db_marker)
    });
    assert!(
        matched,
        "expected a pool_admission timeout row naming {db_marker}, got: {contents}"
    );
}

/// Writer-timeout NDJSON sink coverage contract: a standalone-writer
/// `SQLITE_BUSY`/`SQLITE_LOCKED` error surfaced through `SqlBridge` (the
/// `sql_bridge.rs` emission site) must produce a `"kind":"timeout"` /
/// `"site":"standalone:sql_bridge"` row naming this pool's own database
/// path.
#[tokio::test]
async fn sql_bridge_busy_standalone_writer_emits_ndjson_row() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("sql_bridge_busy_sink_test.db");
    let pool_cfg = PoolConfig {
        path: Some(db_path.clone()),
        busy_timeout: Duration::from_millis(100),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    // Hold the write lock on an independent standalone connection so the
    // bridge's own standalone writer (opened by `SqlBridge::writer()`) is
    // the one that starves and surfaces SQLITE_BUSY.
    let holder = pool.open_standalone_writer().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let result = writer
        .execute(SqlStatement {
            sql: "INSERT INTO t (id) VALUES (1)".to_string(),
            params: Vec::<SqlValue>::new(),
            label: None,
        })
        .await;
    assert!(
        result.is_err(),
        "expected the insert to fail while another connection holds the write lock"
    );

    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);

    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let contents = read_sink_ndjson();
    let matched = contents.lines().any(|line| {
        line.contains("\"kind\":\"timeout\"")
            && line.contains("\"site\":\"standalone:sql_bridge\"")
            && line.contains(&db_marker)
    });
    assert!(
        matched,
        "expected a standalone:sql_bridge busy row naming {db_marker}, got: {contents}"
    );
}
