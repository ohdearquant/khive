//! Periodic WAL checkpoint task for the connection pool.
//!
//! Issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick — including when the
//! WAL page count exceeds the high-water mark. PASSIVE is the only mode the
//! periodic task ever uses.
//!
//! Non-contending design: `checkpoint_once` uses `try_writer_nowait` (zero-wait
//! `try_lock`) so a tick is skipped immediately when any writer holds the mutex,
//! rather than blocking for up to `checkout_timeout`. The checkpoint task must
//! never stall active write traffic — a skipped tick is always preferable.
//!
//! Why TRUNCATE is excluded from the periodic path: TRUNCATE inherits RESTART
//! semantics — it waits for active readers to release their WAL snapshots and
//! invokes the busy handler before acquiring the exclusive lock needed to reset
//! the WAL file. With PoolConfig's 30 s busy_timeout, the task could sit inside
//! SQLite holding the sole writer connection for up to 30 s, stalling all normal
//! write traffic. PASSIVE never waits for readers; it checkpoints as many frames
//! as currently possible and returns promptly. When WAL pressure is sustained
//! (high_water_pages exceeded), the task emits a WARNING so an operator or
//! scheduler can perform a blocking TRUNCATE at a safe moment outside normal
//! traffic.

use std::sync::Arc;
use std::time::Duration;

use crate::pool::ConnectionPool;

/// Configuration for the WAL checkpoint background task.
///
/// All fields default to conservative production values. Override via the
/// environment variables documented on each field.
#[derive(Clone, Debug)]
pub struct CheckpointConfig {
    /// How often to run a passive checkpoint when there is no active write.
    ///
    /// Overridable via `KHIVE_CHECKPOINT_INTERVAL_MS` (milliseconds).
    /// Default: 500 ms.
    pub interval: Duration,

    /// WAL page count above which a warning is logged.
    ///
    /// Overridable via `KHIVE_WAL_WARN_PAGES`.
    /// Default: 2000 pages (~8 MB at 4 KiB page size).
    pub warn_pages: u64,

    /// WAL page count above which a high-pressure WARNING is logged.
    ///
    /// The periodic task always runs PASSIVE regardless; this threshold signals
    /// that a long-lived reader may be pinning an old WAL snapshot that PASSIVE
    /// cannot reclaim. An operator can then schedule a blocking TRUNCATE at a
    /// safe moment outside normal write traffic.
    ///
    /// Overridable via `KHIVE_WAL_HIGH_WATER_PAGES`.
    /// Default: 6000 pages (~24 MB at 4 KiB page size).
    pub high_water_pages: u64,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            warn_pages: 2000,
            high_water_pages: 6000,
        }
    }
}

impl CheckpointConfig {
    /// Build a `CheckpointConfig` from the environment.
    ///
    /// Unset or unparseable variables fall back to the compiled-in defaults.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(ms) = std::env::var("KHIVE_CHECKPOINT_INTERVAL_MS") {
            if let Ok(v) = ms.parse::<u64>() {
                if v > 0 {
                    cfg.interval = Duration::from_millis(v);
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_WARN_PAGES") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.warn_pages = n;
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_HIGH_WATER_PAGES") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.high_water_pages = n;
                }
            }
        }

        cfg
    }
}

/// Run the WAL checkpoint background task.
///
/// This is a long-running async task that should be spawned with
/// `tokio::spawn`. It loops until the pool is dropped (the `Arc` count
/// falls to one, meaning this task holds the last reference).
///
/// The task issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick. PASSIVE is
/// the only checkpoint mode used; see the module-level doc for why TRUNCATE is
/// excluded. A WARNING is emitted once on threshold crossing (wal_pages
/// transitions from below `high_water_pages` to at/above) rather than on every
/// tick, preventing log spam when a long-lived reader pins a WAL snapshot.
///
/// Uses `try_writer_nowait` (zero-wait try-lock) so a busy writer causes the
/// current tick to be skipped rather than stalling write traffic.
pub async fn run_checkpoint_task(pool: Arc<ConnectionPool>, config: CheckpointConfig) {
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_above_high_water = false;

    loop {
        interval.tick().await;

        // Stop looping when this task is the sole Arc holder — the daemon is
        // shutting down and the pool will be dropped imminently.
        if Arc::strong_count(&pool) <= 1 {
            break;
        }

        let wal_pages = checkpoint_once(&pool, &config);
        let above = wal_pages >= config.high_water_pages;
        if above && !was_above_high_water {
            tracing::warn!(
                wal_pages,
                high_water = config.high_water_pages,
                "WAL high-water mark exceeded; sustained WAL pressure — \
                 a long-lived reader may be pinning an old snapshot that PASSIVE cannot reclaim"
            );
        }
        was_above_high_water = above;
    }
}

/// Issue one checkpoint cycle against the writer connection.
///
/// Returns the observed WAL page count (0 when the writer mutex was busy and
/// the tick was skipped, or when the pool is in-memory). All checkpoint errors
/// are logged at warn level and treated as non-fatal; the next tick retries.
///
/// Uses `try_writer_nowait` so that a busy active writer causes this tick to
/// be skipped immediately rather than stalling for up to `checkout_timeout`.
/// The caller (`run_checkpoint_task`) owns threshold-crossing WARN logging so
/// that high-water warnings fire at most once per crossing, not every tick.
pub fn checkpoint_once(pool: &ConnectionPool, config: &CheckpointConfig) -> u64 {
    let writer = match pool.try_writer_nowait() {
        Ok(w) => w,
        Err(_) => return 0,
    };

    let wal_pages = query_wal_pages(writer.conn());

    if wal_pages >= config.warn_pages && wal_pages < config.high_water_pages {
        tracing::warn!(
            wal_pages,
            warn_threshold = config.warn_pages,
            "WAL page count approaching checkpoint threshold"
        );
    }

    if let Err(e) = writer
        .conn()
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
    {
        tracing::warn!(error = %e, "WAL checkpoint failed");
    } else {
        tracing::debug!(wal_pages, "WAL checkpoint issued");
    }

    wal_pages
}

/// Query the current WAL frame count via `PRAGMA wal_checkpoint`.
///
/// The pragma returns a 3-column row `(busy, log, checkpointed)`, where `log`
/// (column index 1) is the number of frames currently in the WAL file — the
/// backlog the high-water threshold keys off. (Column 2 is `checkpointed`, the
/// frames moved *by this call*, which is not the WAL size.) The no-arg pragma
/// also performs a PASSIVE checkpoint as a side effect; the subsequent explicit
/// `PRAGMA wal_checkpoint(PASSIVE)` in `checkpoint_once` is a deliberate second
/// pass that can checkpoint any frames written between the two calls.
///
/// Returns 0 on any error (e.g. in-memory DB where WAL is not active, which
/// reports `log = -1`).
fn query_wal_pages(conn: &rusqlite::Connection) -> u64 {
    conn.query_row("PRAGMA wal_checkpoint", [], |row| row.get::<_, i64>(1))
        .unwrap_or(0)
        .max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;
    use serial_test::serial;

    fn file_pool(path: &std::path::Path) -> Arc<ConnectionPool> {
        let cfg = PoolConfig {
            path: Some(path.to_path_buf()),
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(cfg).expect("pool open"))
    }

    #[test]
    fn checkpoint_once_succeeds_on_file_backed_pool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal_test.db");
        let pool = file_pool(&path);

        // Create a table so the DB is not completely empty.
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);")
                .unwrap();
            writer
                .conn()
                .execute_batch("INSERT INTO t VALUES (1);")
                .unwrap();
        }

        let config = CheckpointConfig::default();
        checkpoint_once(&pool, &config);
    }

    #[test]
    fn checkpoint_once_is_noop_on_in_memory_pool() {
        // In-memory databases do not use WAL; checkpoint_once must not panic.
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(cfg).expect("in-memory pool"));
        let config = CheckpointConfig::default();
        checkpoint_once(&pool, &config);
    }

    #[tokio::test]
    async fn checkpoint_task_exits_when_pool_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal_task_drop.db");
        let pool = file_pool(&path);

        // Use a very short interval so the task ticks quickly in the test.
        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            ..Default::default()
        };

        let weak = Arc::downgrade(&pool);
        let task_pool = Arc::clone(&pool);
        let handle = tokio::spawn(run_checkpoint_task(task_pool, cfg));

        // Drop our copy — only the task holds the Arc now.
        drop(pool);

        // The task detects strong_count == 1 on its next tick and exits.
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        assert!(weak.upgrade().is_none(), "pool should be fully dropped");
    }

    #[test]
    #[serial]
    fn checkpoint_config_env_override() {
        std::env::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "250");
        std::env::set_var("KHIVE_WAL_WARN_PAGES", "1500");
        std::env::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "8000");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");

        assert_eq!(cfg.interval, Duration::from_millis(250));
        assert_eq!(cfg.warn_pages, 1500);
        assert_eq!(cfg.high_water_pages, 8000);
    }

    #[test]
    #[serial]
    fn checkpoint_config_defaults_on_invalid_env() {
        let default = CheckpointConfig::default();

        std::env::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "not_a_number");
        std::env::set_var("KHIVE_WAL_WARN_PAGES", "");
        std::env::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "0");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");

        assert_eq!(cfg.interval, default.interval);
        assert_eq!(cfg.warn_pages, default.warn_pages);
        assert_eq!(cfg.high_water_pages, default.high_water_pages);
    }

    /// Regression: a high-water tick must NOT block behind an active read transaction.
    ///
    /// Isomorphism guarantee: this test FAILS if `checkpoint_once` regresses to
    /// `PRAGMA wal_checkpoint(TRUNCATE)`. Confirmed by reasoning: TRUNCATE inherits
    /// RESTART semantics and will invoke the busy handler (sleeping up to
    /// `busy_timeout`) while waiting for the open reader snapshot to release.
    /// With `busy_timeout = 200ms` a TRUNCATE regression causes the call to take
    /// ~200ms, blowing the <50ms assertion. PASSIVE returns in <1ms even with an
    /// open reader, because PASSIVE never waits for readers.
    ///
    /// Why `busy_timeout = 200ms`: the test pool uses a small busy_timeout so a
    /// TRUNCATE regression fails fast (~200ms), not after the 30s production default.
    #[test]
    fn checkpoint_high_water_does_not_block_behind_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("high_water_test.db");

        // Use a small busy_timeout so a TRUNCATE regression fails fast (~200ms)
        // rather than hanging the test suite for the 30s production default.
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path.clone()),
                busy_timeout: Duration::from_millis(200),
                ..PoolConfig::default()
            })
            .expect("pool open"),
        );

        // Write data so the WAL has frames to checkpoint.
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);",
                )
                .unwrap();
        }

        // Open a reader and start a real read transaction so it holds a WAL
        // snapshot. An idle connection (no BEGIN) does NOT pin frames and would
        // not cause TRUNCATE to wait — the transaction is required for isomorphism.
        let reader = pool.reader().expect("reader");
        reader
            .execute_batch("BEGIN DEFERRED; SELECT * FROM t;")
            .expect("begin read tx");

        // Write another row AFTER the snapshot is established. These new WAL
        // frames are now pinned by the open reader snapshot — TRUNCATE cannot
        // reclaim them without waiting; PASSIVE skips them and returns immediately.
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("INSERT INTO t VALUES (2);")
                .unwrap();
        }

        // high_water_pages = 1 forces the high-water code path for any non-empty WAL.
        let config = CheckpointConfig {
            high_water_pages: 1,
            ..Default::default()
        };

        let start = std::time::Instant::now();
        checkpoint_once(&pool, &config);
        let elapsed = start.elapsed();

        // Commit and release the read snapshot only after checkpoint_once returns.
        reader.execute_batch("COMMIT;").ok();
        drop(reader);

        // PASSIVE returns in <1ms even with an open reader snapshot.
        // A TRUNCATE regression would block ~busy_timeout (200ms) and fail here.
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "checkpoint_once with active reader snapshot took {:?}; \
             expected <50ms (PASSIVE must not block on readers)",
            elapsed
        );
    }

    #[test]
    #[serial]
    fn checkpoint_config_rejects_zero_for_all_fields() {
        let default = CheckpointConfig::default();
        std::env::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "0");
        std::env::set_var("KHIVE_WAL_WARN_PAGES", "0");
        std::env::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "0");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");

        assert_eq!(
            cfg.interval, default.interval,
            "zero interval must fall back to default"
        );
        assert_eq!(
            cfg.warn_pages, default.warn_pages,
            "zero warn_pages must fall back to default"
        );
        assert_eq!(
            cfg.high_water_pages, default.high_water_pages,
            "zero high_water_pages must fall back to default"
        );
    }
}
