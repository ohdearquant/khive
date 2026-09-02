//! Integration coverage for the writer-timeout NDJSON sink's real wiring
//! (`crate::timeout_sink`, wired into `ConnectionPool::writer()` and every
//! standalone-writer busy/locked mapping).
//!
//! The sink is a process-global `OnceLock` by design (one file, one writer
//! thread, per process — see the module's doc comment). That makes it a
//! poor fit for `khive-db`'s main unit-test binary, where hundreds of
//! unrelated tests boot pools of their own: whichever pool boots first
//! anywhere in that process wins the sink's log directory, and many of
//! those pools' directories are `tempfile::tempdir()`s that get deleted the
//! moment their own test function returns. This file is a dedicated
//! integration-test binary (its own process, per Cargo's `tests/`
//! convention) containing only sink-wiring tests that all point the sink at
//! one shared, process-lifetime directory (via [`ensure_sink_dir`]), so they
//! can run concurrently without racing each other for it.
//!
//! The sink hands events to a background writer thread through a bounded
//! channel, so a caller-path emission never blocks on file I/O and a line
//! is not guaranteed to be on disk the instant the triggering call returns.
//! Every assertion here goes through [`wait_for_ndjson_line`], which polls
//! briefly instead of reading once.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once, OnceLock};
use std::time::Duration;

use khive_db::stores::event::SqlEventStore;
use khive_db::stores::graph::SqlGraphStore;
use khive_db::{ConnectionPool, PoolConfig, SqlBridge};
use khive_storage::event::Event;
use khive_storage::types::{Edge, LinkId, SqlStatement, SqlValue, TextDocument};
use khive_storage::{EventStore, GraphStore, SqlAccess};
use khive_types::{EdgeRelation, EventKind, SubstrateKind};
use uuid::Uuid;

const GRAPH_DDL: &str = include_str!("../sql/graph-ddl.sql");
const EVENTS_DDL: &str = include_str!("../sql/events-ddl.sql");

static SINK_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
static SET_ENV: Once = Once::new();

/// Serializes the two slow-write tests' env-override + writer-task-spawn
/// windows: the threshold env var is process-global and read at spawn, so
/// concurrent set/spawn from both tests would let one test's value leak
/// into the other's handle.
static SLOW_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// The sink names its file after this process's own pid (see the module
/// docs' "FILES ARE PER-PROCESS" section) — this test binary and the sink
/// it's exercising share a process, so `std::process::id()` here names the
/// exact same file the sink itself just opened.
fn read_sink_ndjson() -> String {
    let path = ensure_sink_dir().join(format!("writer_timeouts.{}.ndjson", std::process::id()));
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("expected the sink's NDJSON file to exist and be readable at {path:?}: {e}")
    })
}

/// Poll the sink's NDJSON file until a line satisfying `predicate` appears
/// or `timeout` elapses. The writer thread drains its queue asynchronously
/// — emission is a non-blocking enqueue, never a synchronous write — so a
/// caller-triggered event is not guaranteed to be on disk the instant the
/// triggering call returns. This replaces a single read-immediately-after
/// with a short, bounded wait. Returns whatever the file contained at
/// whichever point it stopped polling, so a timed-out caller's assertion
/// failure message still shows the actual contents.
fn wait_for_ndjson_line(predicate: impl Fn(&str) -> bool, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let contents = read_sink_ndjson();
        if contents.lines().any(&predicate) {
            return contents;
        }
        if std::time::Instant::now() > deadline {
            return contents;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read-only inspection must neither start the process-global writer-timeout
/// sink nor consume its one-time claim. Run the assertion in a freshly
/// spawned copy of this integration-test binary because other tests in the
/// parent process intentionally initialize that global sink.
#[test]
fn read_only_pool_does_not_claim_sink_and_later_writable_pool_can() {
    const CHILD_MARKER: &str = "KHIVE_READ_ONLY_SINK_FRESH_PROCESS";

    if std::env::var(CHILD_MARKER).as_deref() != Ok("1") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "read_only_pool_does_not_claim_sink_and_later_writable_pool_can",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env("KHIVE_TEST_HARNESS", "1")
            .env_remove("KHIVE_WRITER_TIMEOUT_SINK_DIR")
            .output()
            .expect("fresh integration-test process must start");
        assert!(
            output.status.success(),
            "fresh-process read-only sink regression failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let read_only_path = dir.path().join("inspection.db");
    {
        let conn = rusqlite::Connection::open(&read_only_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE inspected(id INTEGER PRIMARY KEY);\
             INSERT INTO inspected DEFAULT VALUES;",
        )
        .unwrap();
    }
    let main_before = std::fs::read(&read_only_path).unwrap();
    let mut entries_before = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries_before.sort();

    let read_only_pool = ConnectionPool::new(PoolConfig {
        path: Some(read_only_path.clone()),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .expect("read-only inspection pool must open");
    let count: i64 = read_only_pool
        .reader()
        .unwrap()
        .tracked_conn()
        .query_row("SELECT COUNT(*) FROM inspected", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    // The sink thread performs its first directory creation asynchronously.
    // Wait beyond its one-second drain cadence before proving absence so this
    // cannot pass merely because a wrongly-started writer has not run yet.
    std::thread::sleep(Duration::from_millis(1_500));
    let mut entries_after = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries_after.sort();
    assert_eq!(std::fs::read(&read_only_path).unwrap(), main_before);
    assert_eq!(entries_after, entries_before);
    assert!(
        !dir.path().join(".khive-logs").exists(),
        "a read-only pool must not initialize filesystem-writing diagnostics"
    );

    // A later writable pool in this same fresh process must still be able to
    // claim the global sink, proving the read-only constructor skipped init
    // rather than claiming a permanently inert handle.
    let writable_path = dir.path().join("writable.db");
    let _writable_pool = ConnectionPool::new(PoolConfig {
        path: Some(writable_path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .expect("later writable pool must open");
    let sink_path = dir
        .path()
        .join(".khive-logs")
        .join(format!("writer_timeouts.{}.ndjson", std::process::id()));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let sink_contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&sink_path) {
            if contents.contains("\"kind\":\"startup\"") {
                break contents;
            }
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "later writable pool did not claim the sink at {sink_path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let writable_marker = writable_path
        .canonicalize()
        .unwrap_or(writable_path)
        .display()
        .to_string();
    assert!(
        sink_contents.contains(&writable_marker),
        "startup row must name the later writable pool: {sink_contents}"
    );
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
    let contents = wait_for_ndjson_line(
        |line| {
            line.contains("\"kind\":\"timeout\"")
                && line.contains("\"site\":\"pool_admission\"")
                && line.contains(&db_marker)
        },
        Duration::from_secs(5),
    );
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
        // Force the legacy standalone-writer path so this test keeps
        // exercising the `standalone:sql_bridge` busy site regardless of
        // the file-backed write-queue default.
        write_queue_enabled: Some(false),
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
    let contents = wait_for_ndjson_line(
        |line| {
            line.contains("\"kind\":\"timeout\"")
                && line.contains("\"site\":\"standalone:sql_bridge\"")
                && line.contains(&db_marker)
        },
        Duration::from_secs(5),
    );
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

/// Coverage-note requirement: the standalone-writer busy/locked mapping in
/// `stores/graph.rs`'s `with_writer` must produce a `"kind":"timeout"` /
/// `"site":"standalone:graph"` row when a real held write lock forces
/// `upsert_edge` to see `SQLITE_BUSY`.
#[tokio::test]
async fn graph_busy_standalone_writer_emits_ndjson_row() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("graph_busy_sink_test.db");
    let pool_cfg = PoolConfig {
        path: Some(db_path.clone()),
        busy_timeout: Duration::from_millis(100),
        // Force the legacy standalone-writer path so this test keeps
        // exercising the `standalone:graph` busy site regardless of the
        // file-backed write-queue default.
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(GRAPH_DDL).unwrap();
    }

    let holder = pool.open_standalone_writer().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let store = SqlGraphStore::new_scoped(Arc::clone(&pool), true, "default");
    let edge = Edge {
        id: LinkId(Uuid::new_v4()),
        namespace: "default".to_string(),
        source_id: Uuid::new_v4(),
        target_id: Uuid::new_v4(),
        relation: EdgeRelation::Extends,
        weight: 1.0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let result = store.upsert_edge(edge).await;
    assert!(
        result.is_err(),
        "expected upsert_edge to fail while another connection holds the write lock"
    );

    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);

    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let contents = wait_for_ndjson_line(
        |line| {
            line.contains("\"kind\":\"timeout\"")
                && line.contains("\"site\":\"standalone:graph\"")
                && line.contains(&db_marker)
        },
        Duration::from_secs(5),
    );
    let matched = contents.lines().any(|line| {
        line.contains("\"kind\":\"timeout\"")
            && line.contains("\"site\":\"standalone:graph\"")
            && line.contains(&db_marker)
    });
    assert!(
        matched,
        "expected a standalone:graph busy row naming {db_marker}, got: {contents}"
    );
}

/// Coverage-note requirement: the standalone-writer busy/locked mapping in
/// `stores/event.rs`'s `with_writer` must produce a `"kind":"timeout"` /
/// `"site":"standalone:event"` row when a real held write lock forces
/// `append_event` to see `SQLITE_BUSY`.
#[tokio::test]
async fn event_busy_standalone_writer_emits_ndjson_row() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("event_busy_sink_test.db");
    let pool_cfg = PoolConfig {
        path: Some(db_path.clone()),
        busy_timeout: Duration::from_millis(100),
        // Force the legacy standalone-writer path so this test keeps
        // exercising the `standalone:event` busy site regardless of the
        // file-backed write-queue default.
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }

    let holder = pool.open_standalone_writer().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let store = SqlEventStore::new_scoped(Arc::clone(&pool), true, "default");
    let event = Event::new(
        "default",
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    );
    let result = store.append_event(event).await;
    assert!(
        result.is_err(),
        "expected append_event to fail while another connection holds the write lock"
    );

    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);

    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let contents = wait_for_ndjson_line(
        |line| {
            line.contains("\"kind\":\"timeout\"")
                && line.contains("\"site\":\"standalone:event\"")
                && line.contains(&db_marker)
        },
        Duration::from_secs(5),
    );
    let matched = contents.lines().any(|line| {
        line.contains("\"kind\":\"timeout\"")
            && line.contains("\"site\":\"standalone:event\"")
            && line.contains(&db_marker)
    });
    assert!(
        matched,
        "expected a standalone:event busy row naming {db_marker}, got: {contents}"
    );
}

/// Coverage-note requirement: the standalone-writer busy/locked mapping in
/// `stores/text.rs`'s `with_writer_unmanaged` must produce a
/// `"kind":"timeout"` / `"site":"standalone:text"` row when a real held
/// write lock forces `upsert_document` to see `SQLITE_BUSY`. `Fts5TextSearch`
/// itself is crate-private, so this goes through the public
/// `StorageBackend::sqlite`/`.text()` surface instead of constructing the
/// store type directly. `#[serial]` because, unlike the other tests here,
/// this one must read `KHIVE_BUSY_TIMEOUT_SECS` and `KHIVE_WRITE_QUEUE` off
/// `PoolConfig::default()` (there is no field to override directly through
/// `StorageBackend`) — serializing avoids a hypothetical future test in this
/// file racing the same env vars. `KHIVE_WRITE_QUEUE=0` forces the legacy
/// standalone-writer path so this test keeps exercising the
/// `standalone:text` busy site regardless of the file-backed write-queue
/// default.
///
/// Env restore ordering is safe as written: `StorageBackend::sqlite`
/// (crates/khive-db/src/backend.rs:38-45) builds its `PoolConfig` via
/// `PoolConfig::default()` — which reads `KHIVE_WRITE_QUEUE` synchronously
/// (crates/khive-db/src/pool.rs:140-143) — and passes it to
/// `ConnectionPool::new`, which resolves the `None` preference to a concrete
/// `Some(..)` synchronously before that constructor returns
/// (crates/khive-db/src/pool.rs:483-488). No pool construction on this path
/// is lazy: the resolved `Some(false)` config is already baked in by the time
/// the variables are restored below.
#[tokio::test]
#[serial_test::serial(writer_timeout_sink_busy_env)]
async fn text_busy_standalone_writer_emits_ndjson_row() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("text_busy_sink_test.db");

    let previous_busy_timeout = std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS");
    let previous_write_queue = std::env::var_os("KHIVE_WRITE_QUEUE");
    std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", "1");
    std::env::set_var("KHIVE_WRITE_QUEUE", "0");
    let backend = khive_db::StorageBackend::sqlite(&db_path).expect("file-backed backend");
    match previous_busy_timeout {
        Some(v) => std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", v),
        None => std::env::remove_var("KHIVE_BUSY_TIMEOUT_SECS"),
    }
    match previous_write_queue {
        Some(v) => std::env::set_var("KHIVE_WRITE_QUEUE", v),
        None => std::env::remove_var("KHIVE_WRITE_QUEUE"),
    }
    assert_eq!(
        backend.pool().config().write_queue_enabled,
        Some(false),
        "writable StorageBackend::sqlite must preserve an explicit queue-off override"
    );

    let store = backend.text("wts_busy_test").expect("text search");

    let holder = backend.pool().open_standalone_writer().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let doc = TextDocument {
        subject_id: Uuid::new_v4(),
        kind: SubstrateKind::Entity,
        record_kind: None,
        title: Some("t".to_string()),
        body: "body".to_string(),
        tags: vec![],
        namespace: "default".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    let result = store.upsert_document(doc).await;
    assert!(
        result.is_err(),
        "expected upsert_document to fail while another connection holds the write lock"
    );

    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);

    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let contents = wait_for_ndjson_line(
        |line| {
            line.contains("\"kind\":\"timeout\"")
                && line.contains("\"site\":\"standalone:text\"")
                && line.contains(&db_marker)
        },
        Duration::from_secs(5),
    );
    let matched = contents.lines().any(|line| {
        line.contains("\"kind\":\"timeout\"")
            && line.contains("\"site\":\"standalone:text\"")
            && line.contains(&db_marker)
    });
    assert!(
        matched,
        "expected a standalone:text busy row naming {db_marker}, got: {contents}"
    );
}

/// A queued write whose send-to-reply span meets the slow-write threshold
/// must produce a `"kind":"slow_write"` row carrying `elapsed_ms` and
/// `queue_depth`, naming this pool's own database path. Threshold is forced
/// to 1ms (env override, read at writer-task spawn) and the op itself
/// sleeps 50ms, so the span is guaranteed over-threshold without depending
/// on scheduler timing.
#[tokio::test]
async fn slow_queued_write_emits_slow_write_row() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("slow_write_sink_test.db");
    let handle = {
        let _guard = SLOW_ENV_LOCK.lock().unwrap();
        std::env::set_var("KHIVE_SLOW_WRITE_THRESHOLD_MS", "1");
        let cfg = PoolConfig {
            path: Some(db_path.clone()),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");
        let handle = pool
            .writer_task_handle()
            .expect("runtime is present")
            .expect("write queue enabled must spawn a writer task");
        std::env::remove_var("KHIVE_SLOW_WRITE_THRESHOLD_MS");
        handle
    };

    let value = handle
        .send(|_conn| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(42u8)
        })
        .await
        .expect("queued write should succeed");
    assert_eq!(value, 42);

    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let is_slow_row = |line: &str| {
        line.contains("\"kind\":\"slow_write\"")
            && line.contains("\"elapsed_ms\":")
            && line.contains("\"queue_depth\":")
            && line.contains(&db_marker)
    };
    let contents = wait_for_ndjson_line(is_slow_row, Duration::from_secs(5));
    assert!(
        contents.lines().any(is_slow_row),
        "expected a slow_write row naming {db_marker}, got: {contents}"
    );
}

/// The disable arm: threshold override `0` must spawn a handle that never
/// emits `slow_write`, even for an over-any-threshold op. Uses its own pool
/// and db path so the assertion ("no slow_write row for THIS db") cannot
/// collide with the positive test's rows in the shared sink file.
#[tokio::test]
async fn slow_write_disabled_by_zero_threshold_emits_nothing() {
    ensure_sink_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("slow_write_disabled_test.db");

    // Race note: the positive test sets this var to "1" concurrently. Spawn
    // the writer task inside a scope that forces "0", then restore. The env
    // is process-global, so serialize the two tests' spawn windows with a
    // lock rather than hoping for ordering.
    let handle = {
        let _guard = SLOW_ENV_LOCK.lock().unwrap();
        std::env::set_var("KHIVE_SLOW_WRITE_THRESHOLD_MS", "0");
        let cfg = PoolConfig {
            path: Some(db_path.clone()),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");
        let handle = pool
            .writer_task_handle()
            .expect("runtime is present")
            .expect("write queue enabled must spawn a writer task");
        std::env::remove_var("KHIVE_SLOW_WRITE_THRESHOLD_MS");
        handle
    };

    handle
        .send(|_conn| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        })
        .await
        .expect("queued write should succeed");

    // Give the sink's writer thread a real chance to drain anything the
    // handle might (wrongly) have emitted before asserting absence — an
    // instant read would pass even against a buggy emit still in flight.
    std::thread::sleep(Duration::from_millis(300));
    let canonical_db: PathBuf = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();
    let contents = read_sink_ndjson();
    assert!(
        !contents
            .lines()
            .any(|l| l.contains("\"kind\":\"slow_write\"") && l.contains(&db_marker)),
        "threshold 0 must disable slow_write rows for this db, got: {contents}"
    );
}
