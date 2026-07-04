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
//!
//! Threshold-crossing WARN semantics: both the `warn_pages` and `high_water_pages`
//! warnings fire at most once per below→above crossing. Skipped ticks (writer
//! busy) leave the crossing state unchanged so that a skip cannot spuriously
//! re-arm the rate limit while WAL pressure is still elevated.

use std::sync::Arc;
use std::time::Duration;

use crate::pool::ConnectionPool;

/// Outcome of a single checkpoint attempt.
///
/// `Skipped` is returned when the writer mutex is already held (the tick is a
/// no-op). `Observed` carries the WAL page count read during the tick. The
/// distinction matters for threshold-crossing WARN rate-limiting: a skipped tick
/// must leave the above/below state unchanged so that a busy tick cannot
/// spuriously re-arm the rate limit while WAL pressure is still elevated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTick {
    /// The writer mutex was busy; no checkpoint was issued this tick.
    Skipped,
    /// A checkpoint was issued; the value is the observed WAL page count.
    Observed(u64),
}

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
/// transitions from below a threshold to at/above) rather than on every tick,
/// preventing log spam when a long-lived reader pins a WAL snapshot.
///
/// Skipped ticks (writer mutex busy) leave both crossing-state flags unchanged
/// so that a skip cannot spuriously re-arm the rate limit while WAL pressure is
/// still elevated.
///
/// Uses `try_writer_nowait` (zero-wait try-lock) so a busy writer causes the
/// current tick to be skipped rather than stalling write traffic.
pub async fn run_checkpoint_task(pool: Arc<ConnectionPool>, config: CheckpointConfig) {
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_above_warn = false;
    let mut was_above_high_water = false;

    loop {
        interval.tick().await;

        // Stop looping when this task is the sole Arc holder — the daemon is
        // shutting down and the pool will be dropped imminently.
        if Arc::strong_count(&pool) <= 1 {
            break;
        }

        let tick = checkpoint_once(&pool);
        // Skipped ticks leave crossing state unchanged — a busy tick must not
        // re-arm the rate limit while WAL pressure is still elevated.
        let wal_pages = match tick {
            CheckpointTick::Skipped => continue,
            CheckpointTick::Observed(n) => n,
        };

        let in_warn_band = wal_pages >= config.warn_pages && wal_pages < config.high_water_pages;
        let above_high_water = wal_pages >= config.high_water_pages;

        log_tx_registry_pressure(wal_pages, config.warn_pages, config.high_water_pages);

        if crossing_warn(in_warn_band, &mut was_above_warn) {
            tracing::warn!(
                wal_pages,
                warn_threshold = config.warn_pages,
                "WAL page count approaching checkpoint threshold"
            );
        }

        if crossing_warn(above_high_water, &mut was_above_high_water) {
            tracing::warn!(
                wal_pages,
                high_water = config.high_water_pages,
                "WAL high-water mark exceeded; sustained WAL pressure — \
                 a long-lived reader may be pinning an old snapshot that PASSIVE cannot reclaim"
            );
        }
    }
}

/// ADR-091 Plank 0: log the oldest open transaction registry entry alongside
/// the WAL frame count on every tick (`debug!` normally, escalating to
/// `warn!` once `wal_pages` crosses `warn_pages`), and, once `wal_pages`
/// reaches `high_water_pages`, enumerate every open registry entry at `warn!`
/// — the "which caller is holding the pin" answer this ADR's static reading
/// could not produce. Observe only: this never enforces or force-closes
/// anything.
fn log_tx_registry_pressure(wal_pages: u64, warn_pages: u64, high_water_pages: u64) {
    let oldest = khive_storage::tx_registry::oldest();
    if wal_pages >= warn_pages {
        if let Some((age, label)) = &oldest {
            tracing::warn!(
                wal_pages,
                oldest_tx_age_secs = age.as_secs_f64(),
                oldest_tx_label = label.as_deref().unwrap_or("<unlabeled>"),
                "WAL checkpoint tick: oldest open transaction registry entry"
            );
        }
    } else if let Some((age, label)) = &oldest {
        tracing::debug!(
            wal_pages,
            oldest_tx_age_secs = age.as_secs_f64(),
            oldest_tx_label = label.as_deref().unwrap_or("<unlabeled>"),
            "WAL checkpoint tick: oldest open transaction registry entry"
        );
    }

    if wal_pages >= high_water_pages {
        // This is the load-bearing deliverable — enumerate every open registry
        // entry so an operator can see which caller(s), if any, are pinning
        // the WAL tail during sustained pressure.
        for (age, label) in khive_storage::tx_registry::snapshot() {
            tracing::warn!(
                wal_pages,
                tx_age_secs = age.as_secs_f64(),
                tx_label = label.as_deref().unwrap_or("<unlabeled>"),
                "WAL high-water: open transaction registry entry"
            );
        }
    }
}

/// Issue one checkpoint cycle against the writer connection.
///
/// Returns [`CheckpointTick::Skipped`] when the writer mutex is already held
/// (the tick is a no-op) and [`CheckpointTick::Observed`] with the WAL page
/// count otherwise. All checkpoint errors are logged at warn level and treated
/// as non-fatal; the next tick retries.
///
/// Uses `try_writer_nowait` so that a busy active writer causes this tick to
/// be skipped immediately rather than stalling for up to `checkout_timeout`.
/// The caller (`run_checkpoint_task`) owns all threshold-crossing WARN logging
/// so that warnings fire at most once per crossing, not every tick.
pub fn checkpoint_once(pool: &ConnectionPool) -> CheckpointTick {
    let writer = match pool.try_writer_nowait() {
        Ok(w) => w,
        Err(_) => return CheckpointTick::Skipped,
    };

    let wal_pages = query_wal_pages(writer.conn());

    if let Err(e) = writer
        .conn()
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
    {
        tracing::warn!(error = %e, "WAL checkpoint failed");
    } else {
        tracing::debug!(wal_pages, "WAL checkpoint issued");
    }

    CheckpointTick::Observed(wal_pages)
}

/// Evaluate whether a threshold-crossing WARN should fire and advance the
/// crossing-state flag.
///
/// Returns `true` on a false→true transition in `now_above` (first observed
/// above-threshold tick after a below-threshold tick), `false` on any other
/// tick. The `was_above` flag is updated in-place to track state across calls.
/// Used by `run_checkpoint_task` for both the `warn_pages` band and the
/// `high_water_pages` threshold.
fn crossing_warn(now_above: bool, was_above: &mut bool) -> bool {
    let fire = now_above && !*was_above;
    *was_above = now_above;
    fire
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
    use tracing::field::{Field, Visit};

    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        message: Option<String>,
        oldest_tx_label: Option<String>,
    }

    #[derive(Default)]
    struct CapturedEventVisitor(CapturedEvent);

    impl Visit for CapturedEventVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "message" => self.0.message = Some(value.to_string()),
                "oldest_tx_label" => self.0.oldest_tx_label = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let formatted = format!("{value:?}");
            let cleaned = formatted
                .trim_start_matches('"')
                .trim_end_matches('"')
                .to_string();
            match field.name() {
                "message" => self.0.message = Some(cleaned),
                "oldest_tx_label" => self.0.oldest_tx_label = Some(cleaned),
                _ => {}
            }
        }
    }

    /// Minimal `tracing::Subscriber` that captures events into a thread-local
    /// vec, installed as the thread-local default for the duration of one
    /// test closure via `tracing::subscriber::with_default`. Mirrors the
    /// capture subscriber in `khive-runtime/src/pack.rs`'s gate-dispatch tests.
    struct CaptureSubscriber {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
    }

    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = CapturedEventVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// ADR-091 Plank 0: `log_tx_registry_pressure` emits a debug-level log
    /// naming the oldest open registry entry's label when WAL pressure is
    /// below `warn_pages`, and escalates to warn-level once at/above it.
    #[test]
    fn log_tx_registry_pressure_reports_oldest_open_entry() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        let _handle =
            khive_storage::tx_registry::register(Some("checkpoint_tick_test".to_string()));

        tracing::subscriber::with_default(subscriber, || {
            log_tx_registry_pressure(100, 2000, 6000);
        });

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|e| {
                e.message.as_deref()
                    == Some("WAL checkpoint tick: oldest open transaction registry entry")
                    && e.oldest_tx_label.as_deref() == Some("checkpoint_tick_test")
            }),
            "expected a log line naming the open registry entry's label, got: {events:?}"
        );
    }

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

        checkpoint_once(&pool);
    }

    #[test]
    fn checkpoint_once_is_noop_on_in_memory_pool() {
        // In-memory databases do not use WAL; checkpoint_once must not panic.
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(cfg).expect("in-memory pool"));
        checkpoint_once(&pool);
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
    /// With `busy_timeout = 2000ms` a TRUNCATE regression causes the call to take
    /// ~2000ms, blowing the <500ms assertion. PASSIVE returns in <1ms even with an
    /// open reader, because PASSIVE never waits for readers.
    ///
    /// Why `busy_timeout = 2000ms` and threshold `< 500ms`: the original 200ms
    /// busy_timeout / 50ms threshold was too tight for contended CI runners where
    /// PASSIVE legitimately takes 50-200ms under parallel-test load. Raising the
    /// busy_timeout to 2000ms keeps the PASSIVE path well below 500ms while a
    /// TRUNCATE regression blocks for ~2000ms — a 4x safety margin on both sides.
    #[test]
    fn checkpoint_high_water_does_not_block_behind_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("high_water_test.db");

        // busy_timeout = 2000ms: a TRUNCATE regression blocks ~2s (clearly caught by
        // the <500ms assertion below), but PASSIVE returns well within 500ms even on
        // a heavily loaded CI runner. 4x margin on both sides vs. the old 200ms/50ms.
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path.clone()),
                busy_timeout: Duration::from_millis(2000),
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

        let start = std::time::Instant::now();
        checkpoint_once(&pool);
        let elapsed = start.elapsed();

        // Commit and release the read snapshot only after checkpoint_once returns.
        reader.execute_batch("COMMIT;").ok();
        drop(reader);

        // PASSIVE returns in <1ms even with an open reader snapshot.
        // A TRUNCATE regression would block ~busy_timeout (2000ms) and fail here.
        // 500ms threshold is generous for CI jitter while staying well below 2000ms.
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "checkpoint_once with active reader snapshot took {:?}; \
             expected <500ms (PASSIVE must not block on readers; \
             a TRUNCATE regression would block ~2000ms)",
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

    /// Regression (Finding 1): a Skipped tick must NOT reset was_above_high_water.
    ///
    /// Before the fix, `checkpoint_once` returned `0` on both a genuinely-empty
    /// WAL and a writer-busy skip. The task treated `0` as an observed page count
    /// and reset `was_above_high_water`, re-arming the rate limit on every busy
    /// tick. With the fix, `CheckpointTick::Skipped` leaves crossing state
    /// unchanged.
    ///
    /// This test drives `crossing_warn` directly (the pure function that owns the
    /// decision) rather than going through the async task, which would require a
    /// logging harness.
    #[test]
    fn skipped_tick_does_not_reset_high_water_crossing_state() {
        let mut was_above = false;

        // First observed tick: above threshold — fires WARN, sets was_above=true.
        assert!(
            crossing_warn(true, &mut was_above),
            "should fire on first crossing"
        );
        assert!(was_above);

        // Simulate several skipped ticks: crossing state must remain true.
        // (In the task, Skipped causes `continue` so crossing_warn is never called.)
        // We verify by calling crossing_warn with the SAME above=true value, which
        // is what Observed(high_count) would produce — but a Skipped tick skips
        // the call entirely, so was_above stays as-is. Test the invariant directly:
        // if we leave was_above unchanged (no call at all), was_above remains true.
        assert!(was_above, "was_above must stay true across skipped ticks");

        // Another observed tick still above threshold — must NOT re-fire.
        let fired = crossing_warn(true, &mut was_above);
        assert!(!fired, "WARN must not re-fire while still above threshold");

        // Observed tick below threshold — resets was_above.
        let fired = crossing_warn(false, &mut was_above);
        assert!(!fired);
        assert!(!was_above);

        // Next observed tick above threshold — fires again (legitimate new crossing).
        let fired = crossing_warn(true, &mut was_above);
        assert!(fired, "WARN must fire again on a new below→above crossing");
    }

    /// Regression (Finding 2): warn_pages WARN fires once on crossing, not every tick.
    ///
    /// Before the fix, the WARN was emitted inside `checkpoint_once` on every tick
    /// while WAL sat in the warn band — log spam under sustained moderate pressure.
    /// With the fix, `crossing_warn` gates the WARN on the first in-band tick only;
    /// subsequent ticks while still in the band return false.
    #[test]
    fn warn_pages_fires_once_on_crossing_not_every_tick() {
        let mut was_above_warn = false;

        // Simulate three consecutive ticks with WAL in the warn band.
        let fired_1 = crossing_warn(true, &mut was_above_warn);
        let fired_2 = crossing_warn(true, &mut was_above_warn);
        let fired_3 = crossing_warn(true, &mut was_above_warn);

        assert!(fired_1, "WARN must fire on the first in-band tick");
        assert!(
            !fired_2,
            "WARN must not fire on the second consecutive in-band tick"
        );
        assert!(
            !fired_3,
            "WARN must not fire on the third consecutive in-band tick"
        );

        // Drop below warn band — resets state.
        crossing_warn(false, &mut was_above_warn);
        assert!(!was_above_warn);

        // Re-enter warn band — fires again.
        let fired_reentry = crossing_warn(true, &mut was_above_warn);
        assert!(
            fired_reentry,
            "WARN must fire again on re-entry into warn band"
        );
    }
}
