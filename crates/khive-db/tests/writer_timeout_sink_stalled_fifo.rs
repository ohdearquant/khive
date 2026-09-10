//! Implementation-agnostic companion to `writer_timeout_sink_stalled_writer.rs`.
//! That file is implementation-anchored: it proves *this* design's writer
//! thread can be slow without the caller path noticing, using a delay knob
//! only this implementation exposes. This file instead builds a fixture any
//! implementation would be caught by: a FIFO pre-created at the sink's own
//! predictable path (`writer_timeouts.<pid>.ndjson`), with no reader ever
//! attached. Opening a FIFO for writing blocks until a reader shows up — so
//! any design that opens or writes the sink file directly on a database
//! caller path blocks there and fails the latency bounds below, regardless
//! of how that design is built. This design's caller path never reaches the
//! sink's writer at all (only a bounded, non-blocking channel handoff), so
//! it is unaffected.
//!
//! Isolated in its own integration-test binary (own process) for the same
//! `OnceLock` reason as its siblings: the sink's process-global slot must
//! still be unclaimed when this file's first pool boots.

use std::process::Command;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use khive_db::{ConnectionPool, PoolConfig};

/// Generous enough that a genuine hang (not just a regression that adds
/// caller-path latency) still produces a test *failure* rather than an
/// indefinitely hung test run.
const BOUND_TIMEOUT: Duration = Duration::from_secs(10);

fn point_sink_at_fifo_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", dir.path());

    // `<pid>` here is this test process's own pid — the same process that
    // will boot the pool below and, with it, the sink's writer thread. The
    // sink names its file after `std::process::id()`, so this is the exact
    // path it will try to open.
    let fifo_path = dir
        .path()
        .join(format!("writer_timeouts.{}.ndjson", std::process::id()));
    let status = Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo must be available to run this fixture");
    assert!(
        status.success(),
        "mkfifo must succeed — a failed mkfifo silently degrades this fixture into a no-op \
         instead of a real stall"
    );

    (dir, fifo_path)
}

/// Run `f` on its own thread and wait for it via a channel with
/// `recv_timeout` instead of joining directly — a regression that makes `f`
/// block forever must fail this test, not hang the whole run.
fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(BOUND_TIMEOUT)
        .expect("operation did not complete within the bound — caller path likely blocked on I/O")
}

#[test]
fn sink_never_adds_measurable_latency_when_its_file_is_a_blocked_fifo() {
    let (_dir, fifo_path) = point_sink_at_fifo_dir();

    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("stalled_fifo_sink_test.db");
    let cfg = PoolConfig {
        path: Some(db_path),
        checkout_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    };

    let pool =
        bounded(move || Arc::new(ConnectionPool::new(cfg).expect("file-backed pool should open")));

    let held = pool.writer().expect("first checkout should succeed");
    let pool_for_thread = Arc::clone(&pool);
    let timed_out = bounded(move || pool_for_thread.writer().is_err());
    drop(held);

    assert!(
        timed_out,
        "a second writer checkout while the first is held must time out"
    );

    // Interaction with the rotate-on-open fix: a FIFO is not a regular
    // file, so opening it must never rotate it away. If a future refactor
    // ever rotated non-regular files, the writer thread would instead
    // create and write a fresh, ordinary file at this path, this test's
    // latency assertions above would still incidentally pass, and this
    // fixture would silently stop testing what it claims to. Assert the
    // FIFO is still exactly what's at this path.
    use std::os::unix::fs::FileTypeExt;
    let meta = std::fs::symlink_metadata(&fifo_path).expect("fifo path must still exist");
    assert!(
        meta.file_type().is_fifo(),
        "the FIFO must not have been rotated away or replaced by a regular file"
    );
}
