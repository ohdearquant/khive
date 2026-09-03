//! Companion to `writer_timeout_sink_stalled.rs`'s unwritable-directory arm:
//! this file proves the caller path never reaches the sink's writer even
//! when that writer is genuinely present and working, just slow — an
//! unwritable directory alone can't rule out a design where a slow-but-
//! eventually-successful write on the writer thread somehow leaks back onto
//! the caller path. Isolated in its own integration-test binary (own
//! process) for the same reason as its sibling: the sink's process-global
//! `OnceLock` must still be unclaimed when this file's first pool boots, and
//! this scenario needs its own process-wide `KHIVE_WRITER_TIMEOUT_SINK_WRITE_DELAY_MS`
//! setting that would otherwise collide with any other test's sink config.
//!
//! This test is implementation-anchored: it exercises this specific
//! design's heartbeat-vs-write timing using a delay knob only this
//! implementation exposes. `writer_timeout_sink_stalled_fifo.rs` covers the
//! same latency-bound claim with a design-agnostic fixture (a FIFO with no
//! reader) that any implementation touching the sink file on a caller path
//! would fail, not just this one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_db::{ConnectionPool, PoolConfig};

const WRITE_DELAY_MS: u64 = 5_000;

fn point_sink_at_slow_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", dir.path());
    std::env::set_var(
        "KHIVE_WRITER_TIMEOUT_SINK_WRITE_DELAY_MS",
        WRITE_DELAY_MS.to_string(),
    );
    // Leak the tempdir so it stays alive for the rest of the process — the
    // writer thread keeps writing (slowly) into it for as long as the
    // process runs.
    std::mem::forget(dir);
}

#[test]
fn sink_never_adds_measurable_latency_when_its_writer_is_genuinely_slow() {
    point_sink_at_slow_writer();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("stalled_writer_sink_test.db");
    let cfg = PoolConfig {
        path: Some(db_path),
        checkout_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    };

    // Pool construction must not wait on the writer thread at all — `init`
    // only resolves a path and spawns a thread; even a writer thread that is
    // about to block for `WRITE_DELAY_MS` on its very first write (the
    // startup row) must not slow this down.
    let construct_start = Instant::now();
    let pool = Arc::new(ConnectionPool::new(cfg).expect("file-backed pool should open"));
    let construct_elapsed = construct_start.elapsed();
    assert!(
        construct_elapsed < Duration::from_millis(WRITE_DELAY_MS / 2),
        "pool construction took {construct_elapsed:?} against a {WRITE_DELAY_MS}ms-per-write \
         sink — the sink must never add filesystem-bound latency to pool boot"
    );

    // A genuine writer-admission timeout (held writer + tiny checkout_timeout)
    // must still resolve in close to `checkout_timeout`, not anywhere near
    // `WRITE_DELAY_MS`, even though this exact event is what the (slow)
    // writer thread will eventually try to append.
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
        elapsed < Duration::from_millis(WRITE_DELAY_MS / 2),
        "checkout_timeout was 50ms but writer() took {elapsed:?} against a \
         {WRITE_DELAY_MS}ms-per-write sink — emit_timeout must be a non-blocking enqueue, \
         never blocking on the writer thread's own I/O latency"
    );
}
