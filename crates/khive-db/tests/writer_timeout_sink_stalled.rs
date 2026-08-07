//! The writer-timeout sink must never add measurable latency to a
//! database caller path, even when its log
//! directory is entirely unwritable. Isolated in its own integration-test
//! binary (own process) so the sink's process-global `OnceLock` is
//! guaranteed to still be unclaimed when this file's first pool boots —
//! sharing a process with any other sink test would let whichever pool
//! boots first (possibly with a healthy directory) win the slot instead.

use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_db::{ConnectionPool, PoolConfig};

/// Point the sink at a directory that can never be created: a regular file
/// occupies the path a directory needs, so `create_dir_all` fails on every
/// attempt for the lifetime of this process.
fn point_sink_at_unwritable_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Leak the tempdir so it (and the blocking file inside it) stays alive
    // for the rest of the process — the writer thread retries this path on
    // every wakeup for as long as the process runs.
    let dir_path = dir.keep();
    let blocker = dir_path.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("write blocker");
    let bogus_log_dir = blocker.join("logs");
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", &bogus_log_dir);
}

#[test]
fn sink_never_adds_measurable_latency_when_its_directory_is_unwritable() {
    point_sink_at_unwritable_dir();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("stalled_sink_test.db");
    let cfg = PoolConfig {
        path: Some(db_path),
        checkout_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    };

    // Pool construction itself must not touch the sink's filesystem at all
    // (`init` only resolves a path — pure — and spawns a thread; the thread
    // does its own first `create_dir_all`/`open` on its own schedule).
    let construct_start = Instant::now();
    let pool = Arc::new(ConnectionPool::new(cfg).expect("file-backed pool should open"));
    let construct_elapsed = construct_start.elapsed();
    assert!(
        construct_elapsed < Duration::from_millis(500),
        "pool construction took {construct_elapsed:?} against an unwritable sink directory — \
         the sink must never add filesystem-bound latency to pool boot"
    );

    // A genuine writer-admission timeout (held writer + tiny checkout_timeout)
    // must still resolve in close to `checkout_timeout`, not measurably more,
    // even though `emit_timeout`'s underlying sink can never successfully
    // write anywhere in this test.
    let held = pool.writer().expect("first checkout should succeed");
    let pool_for_thread = Arc::clone(&pool);
    let start = Instant::now();
    let timed_out = std::thread::spawn(move || pool_for_thread.writer().is_err())
        .join()
        .unwrap();
    let elapsed = start.elapsed();
    drop(held);

    assert!(
        timed_out,
        "a second writer checkout while the first is held must time out"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "checkout_timeout was 50ms but writer() took {elapsed:?} against an unwritable sink \
         directory — emit_timeout must be a non-blocking enqueue, never blocking I/O"
    );
}
