//! The writer-timeout sink must never add measurable latency to a
//! database caller path, even when its log
//! directory is entirely unwritable. Isolated in its own integration-test
//! binary (own process) so the sink's process-global `OnceLock` is
//! guaranteed to still be unclaimed when this file's first pool boots —
//! sharing a process with any other sink test would let whichever pool
//! boots first (possibly with a healthy directory) win the slot instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_db::{ConnectionPool, PoolConfig};

const STARTUP_BARRIER_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_STARTUP_BARRIER_DIR";
const HANG_GUARD_TIMEOUT: Duration = Duration::from_secs(10);

struct SinkStartupBarrier {
    _dir: tempfile::TempDir,
    reached: PathBuf,
    release: PathBuf,
    resumed: PathBuf,
}

impl SinkStartupBarrier {
    fn install() -> Self {
        let dir = tempfile::tempdir().expect("startup barrier tempdir");
        std::env::set_var(STARTUP_BARRIER_ENV, dir.path());
        Self {
            reached: dir.path().join("reached"),
            release: dir.path().join("release"),
            resumed: dir.path().join("resumed"),
            _dir: dir,
        }
    }

    fn wait_for(&self, path: &Path, message: &str) {
        let started = Instant::now();
        while !path.exists() {
            assert!(started.elapsed() < HANG_GUARD_TIMEOUT, "{message}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_until_reached(&self) {
        self.wait_for(
            &self.reached,
            "sink writer did not reach the injected startup barrier",
        );
    }

    fn release(&self) {
        std::fs::write(&self.release, b"release").expect("release sink startup barrier");
        self.wait_for(
            &self.resumed,
            "sink writer did not resume from the injected startup barrier",
        );
    }
}

impl Drop for SinkStartupBarrier {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
        std::env::remove_var(STARTUP_BARRIER_ENV);
    }
}

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
    let startup_barrier = SinkStartupBarrier::install();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("stalled_sink_test.db");
    let cfg = PoolConfig {
        path: Some(db_path),
        checkout_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    };

    // Pool construction must finish while the sink writer is paused before
    // its first filesystem operation. The timeout below is only a hang guard;
    // synchronization at the injected barrier, not elapsed time, decides the
    // behavior under test.
    let (constructed_tx, constructed_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = constructed_tx.send(ConnectionPool::new(cfg));
    });
    startup_barrier.wait_until_reached();
    let pool = Arc::new(
        constructed_rx
            .recv_timeout(HANG_GUARD_TIMEOUT)
            .expect("pool construction blocked on the paused sink writer")
            .expect("file-backed pool should open"),
    );
    startup_barrier.release();

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
