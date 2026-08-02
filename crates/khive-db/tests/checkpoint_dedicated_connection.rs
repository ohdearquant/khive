//! Reproducer for the checkpoint-isolation fix: `checkpoint_once` must run
//! on its own dedicated connection and never contend with a concurrent
//! caller's `pool.writer()` admission.
//!
//! Pre-fix, `checkpoint_once` acquired the pool's writer mutex
//! (`try_writer_nowait`) and held it across `PRAGMA wal_checkpoint(PASSIVE)`
//! for the whole tick. Over a large WAL that pragma can run for seconds, so
//! every concurrent `pool.writer()` caller timed out at `checkout_timeout`
//! (production evidence: 22 caller-side admission timeouts on 2026-08-02
//! across the fleet, sustained bursts, against a persistent 64MiB WAL).
//! `PRAGMA wal_checkpoint(PASSIVE)` takes SQLite's CKPT lock, not the WRITE
//! lock — a writer can commit concurrently with a passive checkpoint: the
//! pool-mutex serialization was an application-level constraint SQLite
//! itself never required. This test proves the fixed design no longer
//! imposes it.

use khive_db::checkpoint::{checkpoint_once, TruncateState};
use khive_db::{CheckpointConfig, ConnectionPool, PoolConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fixture floor: the `-wal` file must reach at least this size before the
/// test proceeds, so a vacuous pass (WAL never actually grew, PASSIVE
/// finishes trivially fast either way) fails loudly instead of silently.
const MIN_WAL_BYTES: u64 = 32 * 1024 * 1024;

/// The pool's `checkout_timeout` under test — short, so a regression to the
/// pre-fix pool-mutex serialization times out quickly instead of the test
/// hanging for the production default (5s).
const CHECKOUT_TIMEOUT: Duration = Duration::from_millis(250);

fn wal_path(db_path: &Path) -> PathBuf {
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    PathBuf::from(wal)
}

/// Safety bound on the fattening loop below: 200,000 rows * 64 KiB caps
/// worst-case disk usage at ~12.2 GiB instead of an unbounded loop if the
/// WAL threshold is never reached for some unexpected reason.
const MAX_FATTEN_ROWS: u32 = 200_000;

/// Insert 64 KiB blobs, each its own committed (autocommit) transaction, so
/// `checkpoint_once` has real, checkpointable frames waiting, until the
/// `-wal` file reaches `MIN_WAL_BYTES`. Disables `PRAGMA wal_autocheckpoint`
/// on this connection first — otherwise SQLite's own default auto-checkpoint
/// threshold (~4000 pages / 16 MiB, well under `MIN_WAL_BYTES`) fires on
/// every commit past that point, capping the WAL near that default forever
/// instead of letting it grow, which turns this loop into a runaway
/// insert-and-immediately-reclaim cycle instead of a bounded fixture.
fn fatten_wal(pool: &ConnectionPool, db_path: &Path) {
    let writer = pool.writer().expect("acquire writer to seed the WAL");
    writer
        .conn()
        .execute_batch(
            "PRAGMA wal_autocheckpoint = 0; \
             CREATE TABLE blobs (v BLOB NOT NULL);",
        )
        .expect("disable auto-checkpoint and create table");

    let payload = vec![0xABu8; 64 * 1024];
    let wal = wal_path(db_path);
    for row in 0..MAX_FATTEN_ROWS {
        writer
            .conn()
            .execute("INSERT INTO blobs (v) VALUES (?1)", [payload.as_slice()])
            .expect("insert blob row");
        // Checking file size on every insert is wasteful syscall churn;
        // every 32 rows (~2 MiB) is fine granularity for a 32 MiB target.
        if row % 32 != 0 {
            continue;
        }
        let size = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        if size >= MIN_WAL_BYTES {
            return;
        }
    }
    panic!(
        "fixture safety bound: inserted {MAX_FATTEN_ROWS} rows without the -wal file \
         reaching {MIN_WAL_BYTES} bytes; wal_autocheckpoint may not be disabled as expected"
    );
}

#[test]
fn checkpoint_once_does_not_block_a_concurrent_pool_writer_admission() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fat_wal_checkpoint_isolation.db");

    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            checkout_timeout: CHECKOUT_TIMEOUT,
            ..PoolConfig::default()
        })
        .expect("pool open"),
    );

    fatten_wal(&pool, &path);

    // Fixture's positive control: if the WAL didn't actually grow, the rest
    // of this test would pass vacuously (nothing for PASSIVE to churn
    // through, so there'd be no contention to observe either way).
    let wal_size = std::fs::metadata(wal_path(&path))
        .expect("-wal file must exist after uncheckpointed writes")
        .len();
    assert!(
        wal_size >= MIN_WAL_BYTES,
        "fixture setup failed: -wal file only reached {wal_size} bytes, expected at least \
         {MIN_WAL_BYTES} — this test would be INVALID (not a real regression check) without \
         a genuinely fat WAL for checkpoint_once to churn through"
    );

    // The checkpoint task's dedicated connection, opened exactly the way
    // `run_checkpoint_task` opens its own (`CheckpointConnection::ensure_open`
    // -> `ConnectionPool::open_standalone_writer`).
    let checkpoint_pool = Arc::clone(&pool);
    let dedicated_conn = checkpoint_pool
        .open_standalone_writer()
        .expect("open dedicated checkpoint connection");

    // Thread A: run the real `checkpoint_once` once, against the fat WAL.
    let thread_a = std::thread::spawn(move || {
        checkpoint_once(
            &checkpoint_pool,
            &dedicated_conn,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        )
    });

    // Thread B starts ~50ms after A — enough head start for A to have
    // entered the PASSIVE pragma on a pre-fix build, where it would be
    // holding the pool's writer mutex for the whole multi-second pass.
    std::thread::sleep(Duration::from_millis(50));

    let writer_pool = Arc::clone(&pool);
    let start = Instant::now();
    let thread_b = std::thread::spawn(move || {
        let result = writer_pool.writer();
        (result.is_ok(), start.elapsed())
    });

    let (writer_admitted, elapsed) = thread_b.join().expect("thread B panicked");
    thread_a.join().expect("thread A panicked").expect(
        "checkpoint_once must succeed against a healthy dedicated connection over a fat WAL",
    );

    // Generous margin (structural lock-ownership test, not a latency
    // benchmark): the assertion is that admission never waits behind a
    // checkpoint at all, so it should clear in well under one
    // checkout_timeout, let alone several.
    let generous_bound = CHECKOUT_TIMEOUT * 4;
    assert!(
        writer_admitted,
        "pool.writer() must succeed while checkpoint_once runs concurrently on its own \
         dedicated connection (elapsed: {elapsed:?}); a timeout here means the checkpoint \
         pragma is still serialized behind the pool's writer mutex — the exact regression \
         this test guards against"
    );
    assert!(
        elapsed < generous_bound,
        "pool.writer() took {elapsed:?} to admit a concurrent checkpoint; expected well \
         under {generous_bound:?} since it should never contend with a checkpoint tick at all"
    );
}
