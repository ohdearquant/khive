//! Reproducer for the checkpoint-isolation fix: `checkpoint_once` must run
//! on its own dedicated connection and a concurrent caller's `pool.writer()`
//! ADMISSION must never queue behind it — even during an armed TRUNCATE.
//! This is an admission guarantee, not a claim that TRUNCATE's own
//! SQLite-level write-blocking window disappeared; see below.
//!
//! Pre-fix, `checkpoint_once` acquired the pool's writer mutex
//! (`try_writer_nowait`) and held it across the whole tick: the PASSIVE pass
//! AND, when armed, the TRUNCATE escalation — which busy-waits up to
//! `truncate_busy_timeout` (seconds) on any live read snapshot pinning the
//! WAL. That bounded busy-wait under the pool's writer mutex is the
//! multi-second hold this test reproduces; a PASSIVE pass alone over tens of
//! MiB completes in tens of milliseconds on modern SSDs and does not
//! contend measurably (measured while building this fixture). Production
//! evidence: 22+ caller-side admission timeouts on 2026-08-02 across the
//! fleet, sustained bursts, against a persistent 64MiB WAL pinned by
//! long-lived readers.
//!
//! `PRAGMA wal_checkpoint(PASSIVE)` takes only SQLite's CKPT lock, not the
//! WRITE lock — a writer can commit concurrently with a passive checkpoint.
//! TRUNCATE additionally acquires SQLite's writer lock and still blocks new
//! write transactions, on any connection, for up to `truncate_busy_timeout`
//! while it waits on a pinning reader — that SQLite-level cost is unchanged
//! by this fix. What the pre-fix design added on top was an
//! application-level constraint SQLite itself never required: serializing
//! checkpoint ADMISSION behind the pool's writer mutex, so a caller could not
//! even be admitted to attempt its own write until the checkpoint tick
//! (PASSIVE, or a busy-waiting TRUNCATE) released that mutex. This test
//! arms TRUNCATE deliberately (see `TRUNCATE_BUSY` below) and asserts that
//! `pool.writer()` is still admitted promptly during its busy-wait — proving
//! the fixed design no longer imposes that admission-path constraint, not
//! that TRUNCATE stopped blocking writes at the SQLite level.

use khive_db::checkpoint::{checkpoint_once, TruncateState};
use khive_db::{CheckpointConfig, ConnectionPool, PoolConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the TRUNCATE escalation busy-waits on the pinning reader below.
/// This is the reproduced hold: a PASSIVE pass alone over a 32MiB WAL
/// completes in tens of milliseconds on modern SSDs (measured — too fast to
/// contend with a 250ms admission timeout), but a TRUNCATE armed against a
/// WAL pinned by a live read snapshot busy-waits for this entire duration,
/// and on the pre-fix design it did so while holding the pool's writer
/// mutex. That bounded multi-second hold is the production failure shape
/// (persistent 64MiB WAL pinned by long-lived readers, admission timeouts in
/// bursts).
const TRUNCATE_BUSY: Duration = Duration::from_secs(2);

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
/// `-wal` file reaches `MIN_WAL_BYTES`. The production pool configuration
/// disables connection-local autocheckpoint, so this fixture also fails loud
/// if that writer invariant regresses.
fn fatten_wal(pool: &ConnectionPool, db_path: &Path) {
    let writer = pool.writer().expect("acquire writer to seed the WAL");
    writer
        .conn()
        .execute_batch("CREATE TABLE blobs (v BLOB NOT NULL)")
        .expect("create table");

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

    // A live read snapshot pinning the WAL: TRUNCATE must wait for readers
    // whose snapshot predates the WAL's end, so with this transaction open
    // the armed TRUNCATE below busy-waits for the full TRUNCATE_BUSY bound.
    // (Any standalone connection works; BEGIN + a SELECT materializes the
    // snapshot.)
    let pinning_reader = pool
        .open_standalone_writer()
        .expect("open the pinning-reader connection");
    pinning_reader
        .execute_batch("BEGIN")
        .expect("open the pinning read transaction");
    let _pin: i64 = pinning_reader
        .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
        .expect("materialize the read snapshot");

    // TRUNCATE armed deterministically: threshold trivially crossed, no
    // interval gate, and a busy bound long enough for thread B to land
    // inside the hold window with wide margins.
    let config = CheckpointConfig {
        truncate_high_water_pages: 1,
        truncate_min_interval: Duration::ZERO,
        truncate_busy_timeout: TRUNCATE_BUSY,
        ..CheckpointConfig::default()
    };

    // The checkpoint task's dedicated connection, opened exactly the way
    // `run_checkpoint_task` opens its own (`CheckpointConnection::ensure_open`
    // -> `ConnectionPool::open_standalone_writer`).
    let checkpoint_pool = Arc::clone(&pool);
    let dedicated_conn = checkpoint_pool
        .open_standalone_writer()
        .expect("open dedicated checkpoint connection");

    // Thread A: run the real `checkpoint_once` once — PASSIVE over the fat
    // WAL, then the armed TRUNCATE busy-waiting ~TRUNCATE_BUSY on the
    // pinning reader.
    let thread_a = std::thread::spawn(move || {
        checkpoint_once(
            &checkpoint_pool,
            &dedicated_conn,
            &config,
            &mut TruncateState::default(),
        )
    });

    // Observe, rather than time-guess, the moment thread A's TRUNCATE actually
    // arms and starts holding SQLite's writer lock: a separate probe
    // connection with `busy_timeout=0` attempts `BEGIN IMMEDIATE` in a tight
    // poll. While no writer lock is held, the probe's own `BEGIN IMMEDIATE`
    // succeeds immediately (and is rolled back to release it before the next
    // attempt); the instant it instead fails with `SQLITE_BUSY`, that failure
    // *is* the observation that thread A now holds the writer lock — not an
    // inference from elapsed wall-clock time. Bounded by a 5s deadline so a
    // TRUNCATE that never arms (a fixture regression) fails loudly instead of
    // this test passing vacuously.
    let probe = pool
        .open_standalone_writer()
        .expect("open the TRUNCATE-arming probe connection");
    probe
        .busy_timeout(Duration::ZERO)
        .expect("set zero busy_timeout on the probe connection");

    let handshake_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match probe.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {
                probe
                    .execute_batch("ROLLBACK")
                    .expect("release the probe's own BEGIN IMMEDIATE");
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                break;
            }
            Err(err) => panic!(
                "probe's BEGIN IMMEDIATE failed with an unexpected error (expected \
                 SQLITE_BUSY): {err:?}"
            ),
        }
        if Instant::now() >= handshake_deadline {
            panic!(
                "fixture INVALID: TRUNCATE never armed within 5s — the probe's BEGIN \
                 IMMEDIATE kept succeeding the whole time, meaning thread A's \
                 checkpoint_once never held SQLite's writer lock; this test would \
                 otherwise pass vacuously without ever observing the hold it exists to \
                 reproduce"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

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
