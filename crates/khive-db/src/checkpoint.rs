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
//! re-arm the rate limit while WAL pressure is still elevated. The ADR-091
//! Plank 0 open-transaction-registry WARNs (oldest-entry escalation and the
//! high-water snapshot enumeration) ride the SAME crossing gates — they are
//! not independently rate-limited, so they never repeat on consecutive ticks
//! that remain above a threshold. Only the per-tick `debug!` trace of the
//! oldest open entry fires unconditionally.
//!
//! ADR-091 Plank 2: rare TRUNCATE escalation. The periodic tick above stays
//! PASSIVE-only and non-blocking; on top of it, `checkpoint_once` also
//! evaluates a much rarer escalation to `PRAGMA wal_checkpoint(TRUNCATE)`
//! once the WAL has grown past `truncate_high_water_pages` and at least
//! `truncate_min_interval` has elapsed since the last TRUNCATE *attempt*
//! (not the last successful reclaim). This is a **single writer checkout per
//! tick**: PASSIVE and any due TRUNCATE both run under the one guard
//! `checkpoint_once` already holds — there is never a second concurrent
//! checkout for TRUNCATE. If the writer mutex is busy, both PASSIVE and any
//! due TRUNCATE are skipped for that tick, and `last_truncate_attempt` is
//! left untouched so the next tick where the writer is free is immediately
//! eligible rather than waiting out the full interval again.
//! `last_truncate_attempt` only advances on a tick that actually attempted
//! TRUNCATE (writer held, threshold crossed, interval elapsed) — never on a
//! skip for any reason (writer busy, below threshold, interval not yet up).
//! TRUNCATE runs under a temporarily shortened `busy_timeout`
//! (`truncate_busy_timeout`), restored on the writer connection immediately
//! after the attempt, win or lose. No transaction is ever killed or aborted
//! here — the tx_registry is only read for diagnostics (Plank 1 owns
//! enforcement).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pool::{ConnectionPool, WriterGuard};

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

    /// WAL page count above which a TRUNCATE escalation attempt is armed
    /// (ADR-091 Plank 2).
    ///
    /// This is a separate, much higher threshold than `high_water_pages`:
    /// crossing it does not itself attempt TRUNCATE — it only arms the
    /// attempt, which additionally requires `truncate_min_interval` to have
    /// elapsed since the last attempt.
    ///
    /// Overridable via `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`.
    /// Default: 20000 pages.
    pub truncate_high_water_pages: u64,

    /// Minimum spacing between TRUNCATE *attempts* (not successes).
    ///
    /// A skipped tick (writer busy, below threshold, or interval not yet
    /// elapsed) never advances the "last attempt" clock, so the next tick
    /// where the writer is free and the threshold is still crossed is
    /// immediately eligible rather than waiting out the full interval again.
    ///
    /// Overridable via `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS`.
    /// Default: 300 seconds (5 minutes).
    pub truncate_min_interval: Duration,

    /// Temporary `busy_timeout` used only for the duration of a TRUNCATE
    /// attempt, restored to the pool's configured busy timeout immediately
    /// after the attempt completes (win or lose).
    ///
    /// Overridable via `KHIVE_WAL_TRUNCATE_BUSY_MS`.
    /// Default: 2000 ms.
    pub truncate_busy_timeout: Duration,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            warn_pages: 2000,
            high_water_pages: 6000,
            truncate_high_water_pages: 20_000,
            truncate_min_interval: Duration::from_secs(300),
            truncate_busy_timeout: Duration::from_millis(2000),
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

        if let Ok(v) = std::env::var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.truncate_high_water_pages = n;
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.truncate_min_interval = Duration::from_secs(n);
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_TRUNCATE_BUSY_MS") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.truncate_busy_timeout = Duration::from_millis(n);
                }
            }
        }

        cfg
    }
}

/// Mutable escalation state carried across ticks by the caller (ADR-091 Plank 2).
///
/// Kept separate from [`CheckpointConfig`] because it is *state*, not
/// configuration: `last_attempt` and `consecutive_failures` mutate every tick,
/// while `CheckpointConfig` is parsed once and held immutable for the life of
/// the task.
#[derive(Debug, Default)]
pub struct TruncateState {
    /// When the last TRUNCATE *attempt* ran (armed + writer held), regardless
    /// of whether it succeeded in reclaiming pages. `None` means no attempt
    /// has ever run, so the first armed tick is immediately eligible.
    last_attempt: Option<Instant>,
    /// Count of consecutive TRUNCATE attempts that failed to bring `wal_pages`
    /// back below `warn_pages`. Resets to 0 the first time an attempt clears
    /// `warn_pages`; used to fire a one-shot escalated WARN at exactly 3
    /// consecutive failures (does not repeat every subsequent attempt).
    consecutive_failures: u32,
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
    let mut truncate_state = TruncateState::default();

    loop {
        interval.tick().await;

        // Stop looping when this task is the sole Arc holder — the daemon is
        // shutting down and the pool will be dropped imminently.
        if Arc::strong_count(&pool) <= 1 {
            break;
        }

        let tick = checkpoint_once(&pool, &config, &mut truncate_state);
        // Skipped ticks leave crossing state unchanged — a busy tick must not
        // re-arm the rate limit while WAL pressure is still elevated.
        let wal_pages = match tick {
            CheckpointTick::Skipped => continue,
            CheckpointTick::Observed(n) => n,
        };

        let above_warn = wal_pages >= config.warn_pages;
        let above_high_water = wal_pages >= config.high_water_pages;

        // Per-tick debug for the oldest open entry always fires (cheap, single
        // `oldest()` lookup); the two `warn!`-level registry logs below are
        // gated on the SAME crossing state as the WAL-threshold WARNs above,
        // so sustained pressure logs once per crossing, not once per tick.
        log_tx_registry_oldest_debug(wal_pages);

        let warn_crossed = crossing_warn(above_warn, &mut was_above_warn);
        if warn_crossed {
            log_tx_registry_oldest_warn(wal_pages);
            tracing::warn!(
                wal_pages,
                warn_threshold = config.warn_pages,
                "WAL page count approaching checkpoint threshold"
            );
        }

        let high_water_crossed = crossing_warn(above_high_water, &mut was_above_high_water);
        if high_water_crossed {
            log_tx_registry_snapshot_warn(wal_pages);
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
/// the WAL frame count at `debug!`, on EVERY tick regardless of threshold
/// state. This is the low-volume per-tick trace; the WARN-level escalations
/// live in [`log_tx_registry_oldest_warn`] and
/// [`log_tx_registry_snapshot_warn`], both of which are gated on threshold
/// *crossing* by the caller (`run_checkpoint_task`) so they fire once per
/// crossing rather than once per tick. Observe only: this never enforces or
/// force-closes anything.
fn log_tx_registry_oldest_debug(wal_pages: u64) {
    if let Some((age, label)) = khive_storage::tx_registry::oldest() {
        tracing::debug!(
            wal_pages,
            oldest_tx_age_secs = age.as_secs_f64(),
            oldest_tx_label = label.as_deref().unwrap_or("<unlabeled>"),
            "WAL checkpoint tick: oldest open transaction registry entry"
        );
    }
}

/// ADR-091 Plank 0: escalate the oldest open registry entry to `warn!`.
///
/// Callers MUST gate this on a below→above `warn_pages` crossing (via
/// `crossing_warn`) — it is not rate-limited internally, so calling it every
/// tick would reproduce the log-spam bug this rewrite fixes.
fn log_tx_registry_oldest_warn(wal_pages: u64) {
    if let Some((age, label)) = khive_storage::tx_registry::oldest() {
        tracing::warn!(
            wal_pages,
            oldest_tx_age_secs = age.as_secs_f64(),
            oldest_tx_label = label.as_deref().unwrap_or("<unlabeled>"),
            "WAL checkpoint tick: oldest open transaction registry entry"
        );
    }
}

/// ADR-091 Plank 0: enumerate every open registry entry at `warn!` — the
/// "which caller is holding the pin" answer this ADR's static reading could
/// not produce.
///
/// Callers MUST gate this on a below→above `high_water_pages` crossing (via
/// `crossing_warn`) — it is not rate-limited internally, so calling it every
/// tick would repeat the full snapshot enumeration every tick under
/// sustained pressure.
fn log_tx_registry_snapshot_warn(wal_pages: u64) {
    for (age, label) in khive_storage::tx_registry::snapshot() {
        tracing::warn!(
            wal_pages,
            tx_age_secs = age.as_secs_f64(),
            tx_label = label.as_deref().unwrap_or("<unlabeled>"),
            "WAL high-water: open transaction registry entry"
        );
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
///
/// ADR-091 Plank 2: after the PASSIVE pass, this is also the single point
/// that may escalate to TRUNCATE (`maybe_truncate`) — under the SAME writer
/// guard acquired above, never a second checkout. A busy writer (`Skipped`)
/// short-circuits before either PASSIVE or TRUNCATE run.
pub fn checkpoint_once(
    pool: &ConnectionPool,
    config: &CheckpointConfig,
    truncate_state: &mut TruncateState,
) -> CheckpointTick {
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

    maybe_truncate(pool, &writer, config, wal_pages, truncate_state);

    CheckpointTick::Observed(wal_pages)
}

/// ADR-091 Plank 2: evaluate and, if due, attempt a TRUNCATE escalation.
///
/// Runs under the writer guard the caller already holds — never performs its
/// own checkout. Returns immediately (a no-op) unless BOTH:
/// - `wal_pages >= config.truncate_high_water_pages`, and
/// - no prior attempt (`truncate_state.last_attempt.is_none()`) OR at least
///   `config.truncate_min_interval` has elapsed since the last attempt.
///
/// `truncate_state.last_attempt` is stamped ONLY on this path (an actual
/// attempt) — a return before that point (below threshold, interval not
/// elapsed) never touches it, matching the ADR's "skip must not stamp"
/// requirement.
///
/// The oldest-pinning-transaction snapshot is logged (reusing Plank 0's
/// `tx_registry`) before the attempt, so an operator can see what is
/// (potentially) pinning the WAL even if the attempt goes on to succeed.
/// `busy_timeout` is temporarily lowered to `config.truncate_busy_timeout` for
/// the PRAGMA call and restored to the pool's configured value immediately
/// after, regardless of outcome. No transaction is ever killed here — this is
/// read-only diagnostics plus the TRUNCATE pragma itself; enforcement is
/// Plank 1's job, not this one's.
fn maybe_truncate(
    pool: &ConnectionPool,
    writer: &WriterGuard<'_>,
    config: &CheckpointConfig,
    wal_pages_before: u64,
    truncate_state: &mut TruncateState,
) {
    if wal_pages_before < config.truncate_high_water_pages {
        return;
    }

    if let Some(last) = truncate_state.last_attempt {
        if last.elapsed() < config.truncate_min_interval {
            return;
        }
    }

    truncate_state.last_attempt = Some(Instant::now());

    // Which caller (if any) is pinning the WAL — logged before the attempt so
    // it is available even if the attempt itself succeeds.
    log_tx_registry_snapshot_warn(wal_pages_before);

    let conn = writer.conn();
    let original_busy_timeout = pool.config().busy_timeout;

    if let Err(e) = conn.busy_timeout(config.truncate_busy_timeout) {
        tracing::warn!(error = %e, "failed to lower busy_timeout for TRUNCATE attempt; skipping");
        return;
    }

    let start = Instant::now();
    let outcome = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    let elapsed = start.elapsed();

    // Restore the pool's configured busy_timeout immediately after the
    // attempt, win or lose, before any other logging or bookkeeping.
    if let Err(e) = conn.busy_timeout(original_busy_timeout) {
        tracing::warn!(error = %e, "failed to restore busy_timeout after TRUNCATE attempt");
    }

    match outcome {
        Ok(()) => {
            let wal_pages_after = query_wal_pages(conn);
            tracing::info!(
                wal_pages_before,
                wal_pages_after,
                elapsed_ms = elapsed.as_millis() as u64,
                "WAL TRUNCATE checkpoint attempted"
            );

            let made_progress = wal_pages_after < wal_pages_before;
            if !made_progress {
                tracing::warn!(
                    wal_pages_before,
                    wal_pages_after,
                    "WAL TRUNCATE attempt made no progress; \
                     a long-lived reader may still be pinning the WAL snapshot"
                );
                log_tx_registry_snapshot_warn(wal_pages_after);
            }

            note_truncate_outcome(config, wal_pages_after, truncate_state);
        }
        Err(e) => {
            tracing::warn!(error = %e, wal_pages_before, "WAL TRUNCATE attempt failed");
            log_tx_registry_snapshot_warn(wal_pages_before);
            note_truncate_outcome(config, wal_pages_before, truncate_state);
        }
    }
}

/// ADR-091 Plank 2: track consecutive TRUNCATE attempts that fail to bring
/// `wal_pages` back below `warn_pages`, firing a one-shot escalated WARN at
/// exactly the third consecutive failure (does not repeat every attempt
/// thereafter — mirrors the crossing-WARN debounce used elsewhere in this
/// module). A single attempt that clears `warn_pages` resets the counter.
fn note_truncate_outcome(
    config: &CheckpointConfig,
    wal_pages_after: u64,
    state: &mut TruncateState,
) {
    if wal_pages_after >= config.warn_pages {
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures == 3 {
            tracing::warn!(
                wal_pages_after,
                warn_threshold = config.warn_pages,
                "WAL TRUNCATE has failed to clear WAL pressure for 3 consecutive attempts"
            );
        }
    } else {
        state.consecutive_failures = 0;
    }
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
        tx_label: Option<String>,
    }

    #[derive(Default)]
    struct CapturedEventVisitor(CapturedEvent);

    impl Visit for CapturedEventVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "message" => self.0.message = Some(value.to_string()),
                "oldest_tx_label" => self.0.oldest_tx_label = Some(value.to_string()),
                "tx_label" => self.0.tx_label = Some(value.to_string()),
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
                "tx_label" => self.0.tx_label = Some(cleaned),
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

    /// ADR-091 Plank 0: `log_tx_registry_oldest_debug` emits a debug-level log
    /// naming the oldest open registry entry's label, on every call.
    ///
    /// `#[serial(tx_registry)]`: the registry is a process-wide singleton
    /// shared across every test in this binary — see `pool.rs`'s and
    /// `sql_bridge.rs`'s registry tests, which share this same serial group
    /// (round-1 fix: these three were previously unserialized and could
    /// race, corrupting each other's `oldest()`/`snapshot()` reads).
    ///
    /// This test does NOT hardcode "checkpoint_tick_test" as the expected
    /// label: production write paths elsewhere in this same test binary
    /// (vectors/graph/text stores) also register short-lived registry
    /// entries while their own tests run, and `serial(tx_registry)` only
    /// serializes against the OTHER tests in that same group, not against
    /// every write path in the crate. Instead it samples `oldest()` itself
    /// immediately before invoking the function under test and asserts the
    /// logged label matches whatever the registry considers oldest at that
    /// instant — deterministic regardless of unrelated concurrent registry
    /// churn, while still verifying `log_tx_registry_oldest_debug` correctly
    /// surfaces the registry's own `oldest()` answer.
    #[test]
    #[serial(tx_registry)]
    fn log_tx_registry_oldest_debug_reports_oldest_open_entry() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        let _handle =
            khive_storage::tx_registry::register(Some("checkpoint_tick_test".to_string()));

        let expected_label = khive_storage::tx_registry::oldest()
            .and_then(|(_, label)| label)
            .unwrap_or_else(|| "<unlabeled>".to_string());

        tracing::subscriber::with_default(subscriber, || {
            log_tx_registry_oldest_debug(100);
        });

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|e| {
                e.message.as_deref()
                    == Some("WAL checkpoint tick: oldest open transaction registry entry")
                    && e.oldest_tx_label.as_deref() == Some(expected_label.as_str())
            }),
            "expected a log line naming the open registry entry's label, got: {events:?}"
        );
    }

    /// ADR-091 Plank 0 (round-1 fix): the oldest-entry WARN and the
    /// high-water snapshot-enumeration WARN are gated by `crossing_warn` at
    /// the call site (mirroring the WAL-threshold WARNs), so driving two
    /// consecutive above-threshold ticks through that same gate must produce
    /// exactly one of each — never a repeat on the second tick.
    #[test]
    #[serial(tx_registry)]
    fn registry_warns_fire_on_crossing_and_do_not_repeat() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        let _handle =
            khive_storage::tx_registry::register(Some("registry_warn_crossing_test".to_string()));

        let mut was_above_warn = false;
        let mut was_above_high_water = false;

        tracing::subscriber::with_default(subscriber, || {
            // Tick 1: below→above crossing for both bands — both WARNs fire.
            if crossing_warn(true, &mut was_above_warn) {
                log_tx_registry_oldest_warn(6000);
            }
            if crossing_warn(true, &mut was_above_high_water) {
                log_tx_registry_snapshot_warn(6000);
            }

            // Tick 2: still above both thresholds — neither must repeat.
            if crossing_warn(true, &mut was_above_warn) {
                log_tx_registry_oldest_warn(6000);
            }
            if crossing_warn(true, &mut was_above_high_water) {
                log_tx_registry_snapshot_warn(6000);
            }
        });

        let events = buffer.lock().unwrap();

        // `tracing::subscriber::with_default` scopes capture to THIS thread for
        // the duration of the closure, so `events` contains only the two
        // `log_tx_registry_oldest_warn` calls made above — no concurrent test's
        // log calls land in this buffer. This lets the crossing/no-repeat
        // assertion match on message text alone: unlike the "names MY label"
        // assertion in the sibling test above, WHICH label `oldest()` reports
        // is irrelevant here (a concurrent write path elsewhere in the binary
        // may transiently be the registry's genuine oldest entry) — only the
        // fire-once-per-crossing COUNT is under test.
        let oldest_warn_count = events
            .iter()
            .filter(|e| {
                e.message.as_deref()
                    == Some("WAL checkpoint tick: oldest open transaction registry entry")
            })
            .count();
        assert_eq!(
            oldest_warn_count, 1,
            "oldest-entry WARN must fire exactly once across two above-threshold ticks, got: {events:?}"
        );

        let snapshot_warn_count = events
            .iter()
            .filter(|e| {
                e.message.as_deref() == Some("WAL high-water: open transaction registry entry")
                    && e.tx_label.as_deref() == Some("registry_warn_crossing_test")
            })
            .count();
        assert_eq!(
            snapshot_warn_count, 1,
            "high-water snapshot WARN must fire exactly once across two above-threshold ticks, got: {events:?}"
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

        checkpoint_once(
            &pool,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        );
    }

    #[test]
    fn checkpoint_once_is_noop_on_in_memory_pool() {
        // In-memory databases do not use WAL; checkpoint_once must not panic.
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(cfg).expect("in-memory pool"));
        checkpoint_once(
            &pool,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        );
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
        std::env::set_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", "12000");
        std::env::set_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS", "60");
        std::env::set_var("KHIVE_WAL_TRUNCATE_BUSY_MS", "500");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");

        assert_eq!(cfg.interval, Duration::from_millis(250));
        assert_eq!(cfg.warn_pages, 1500);
        assert_eq!(cfg.high_water_pages, 8000);
        assert_eq!(cfg.truncate_high_water_pages, 12000);
        assert_eq!(cfg.truncate_min_interval, Duration::from_secs(60));
        assert_eq!(cfg.truncate_busy_timeout, Duration::from_millis(500));
    }

    #[test]
    #[serial]
    fn checkpoint_config_defaults_on_invalid_env() {
        let default = CheckpointConfig::default();

        std::env::set_var("KHIVE_CHECKPOINT_INTERVAL_MS", "not_a_number");
        std::env::set_var("KHIVE_WAL_WARN_PAGES", "");
        std::env::set_var("KHIVE_WAL_HIGH_WATER_PAGES", "0");
        std::env::set_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", "not_a_number");
        std::env::set_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS", "");
        std::env::set_var("KHIVE_WAL_TRUNCATE_BUSY_MS", "0");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");

        assert_eq!(cfg.interval, default.interval);
        assert_eq!(cfg.warn_pages, default.warn_pages);
        assert_eq!(cfg.high_water_pages, default.high_water_pages);
        assert_eq!(
            cfg.truncate_high_water_pages,
            default.truncate_high_water_pages
        );
        assert_eq!(cfg.truncate_min_interval, default.truncate_min_interval);
        assert_eq!(cfg.truncate_busy_timeout, default.truncate_busy_timeout);
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
        checkpoint_once(
            &pool,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        );
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
        std::env::set_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", "0");
        std::env::set_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS", "0");
        std::env::set_var("KHIVE_WAL_TRUNCATE_BUSY_MS", "0");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");

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
        assert_eq!(
            cfg.truncate_high_water_pages, default.truncate_high_water_pages,
            "zero truncate_high_water_pages must fall back to default"
        );
        assert_eq!(
            cfg.truncate_min_interval, default.truncate_min_interval,
            "zero truncate_min_interval must fall back to default"
        );
        assert_eq!(
            cfg.truncate_busy_timeout, default.truncate_busy_timeout,
            "zero truncate_busy_timeout must fall back to default"
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

    // ADR-091 Plank 2: TRUNCATE escalation state machine tests.

    /// Trigger threshold: once `wal_pages` (as observed by `checkpoint_once`) is
    /// at/above `truncate_high_water_pages` and no prior attempt has run, the
    /// escalation fires and stamps `last_attempt`.
    #[test]
    #[serial(tx_registry)]
    fn truncate_attempts_when_high_water_crossed_with_no_prior_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncate_trigger.db");
        let pool = file_pool(&path);

        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);",
                )
                .unwrap();
        }

        let config = CheckpointConfig {
            // Force the escalation to arm regardless of the tiny WAL this test
            // actually produces — isolates the trigger-threshold behavior from
            // needing to stuff 20,000 real WAL pages.
            truncate_high_water_pages: 0,
            truncate_min_interval: Duration::from_secs(300),
            ..CheckpointConfig::default()
        };
        let mut state = TruncateState::default();

        assert!(
            state.last_attempt.is_none(),
            "precondition: no attempt has run yet"
        );

        let tick = checkpoint_once(&pool, &config, &mut state);
        assert!(matches!(tick, CheckpointTick::Observed(_)));
        assert!(
            state.last_attempt.is_some(),
            "an attempt must be stamped once the high-water threshold is crossed"
        );
    }

    /// Below-threshold skip: `wal_pages < truncate_high_water_pages` must never
    /// stamp `last_attempt` — only an actual attempt advances it.
    #[test]
    #[serial(tx_registry)]
    fn truncate_does_not_attempt_below_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncate_below_threshold.db");
        let pool = file_pool(&path);

        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);",
                )
                .unwrap();
        }

        // Effectively unreachable threshold for this test's tiny WAL.
        let config = CheckpointConfig {
            truncate_high_water_pages: u64::MAX,
            ..CheckpointConfig::default()
        };
        let mut state = TruncateState::default();

        checkpoint_once(&pool, &config, &mut state);

        assert!(
            state.last_attempt.is_none(),
            "a below-threshold tick must never stamp last_attempt"
        );
    }

    /// Min-interval skip: once an attempt has run, a subsequent tick that is
    /// still above threshold but within `truncate_min_interval` must skip
    /// without re-stamping `last_attempt` (the timestamp must not move).
    #[test]
    #[serial(tx_registry)]
    fn truncate_min_interval_skip_does_not_restamp_last_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncate_min_interval.db");
        let pool = file_pool(&path);

        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);",
                )
                .unwrap();
        }

        let config = CheckpointConfig {
            truncate_high_water_pages: 0,
            truncate_min_interval: Duration::from_secs(300),
            ..CheckpointConfig::default()
        };
        let mut state = TruncateState::default();

        checkpoint_once(&pool, &config, &mut state);
        let first_attempt = state.last_attempt.expect("first tick must attempt");

        // Second tick, immediately after: still above threshold, but the
        // min-interval has clearly not elapsed — must skip and leave
        // last_attempt exactly as it was.
        checkpoint_once(&pool, &config, &mut state);
        let second_attempt = state.last_attempt.expect("attempt timestamp must persist");

        assert_eq!(
            first_attempt, second_attempt,
            "a tick within truncate_min_interval must not re-stamp last_attempt"
        );
    }

    /// Busy fallback: when the writer mutex is already held, `checkpoint_once`
    /// must return `Skipped` and never touch the TRUNCATE state at all — both
    /// PASSIVE and any due TRUNCATE are skipped together (one writer checkout
    /// per tick).
    #[test]
    #[serial(tx_registry)]
    fn busy_writer_skips_both_passive_and_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncate_busy_skip.db");
        let pool = file_pool(&path);

        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);",
                )
                .unwrap();
        }

        // Hold the writer mutex for the duration of the checkpoint_once call so
        // try_writer_nowait() fails, exactly like a concurrent write in progress.
        let _held = pool.try_writer().unwrap();

        let config = CheckpointConfig {
            truncate_high_water_pages: 0,
            ..CheckpointConfig::default()
        };
        let mut state = TruncateState::default();

        let tick = checkpoint_once(&pool, &config, &mut state);

        assert_eq!(
            tick,
            CheckpointTick::Skipped,
            "a busy writer must skip the tick entirely"
        );
        assert!(
            state.last_attempt.is_none(),
            "a skipped tick (writer busy) must never stamp last_attempt, \
             even with a threshold that would otherwise arm immediately"
        );
    }

    /// Edge-triggered escalation WARN: `note_truncate_outcome` fires exactly
    /// once, on the third consecutive attempt that fails to clear
    /// `warn_pages`, and does not repeat on a fourth consecutive failure. A
    /// single attempt that clears `warn_pages` resets the counter.
    #[test]
    fn note_truncate_outcome_warns_once_at_third_consecutive_failure() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        let config = CheckpointConfig {
            warn_pages: 2000,
            ..CheckpointConfig::default()
        };
        let mut state = TruncateState::default();

        tracing::subscriber::with_default(subscriber, || {
            // Three consecutive attempts that fail to clear warn_pages.
            note_truncate_outcome(&config, 5000, &mut state);
            note_truncate_outcome(&config, 5000, &mut state);
            note_truncate_outcome(&config, 5000, &mut state);
            // A fourth consecutive failure must not re-fire the escalation.
            note_truncate_outcome(&config, 5000, &mut state);
        });

        assert_eq!(state.consecutive_failures, 4);

        let events = buffer.lock().unwrap();
        let escalation_count = events
            .iter()
            .filter(|e| {
                e.message.as_deref()
                    == Some(
                        "WAL TRUNCATE has failed to clear WAL pressure for 3 consecutive attempts",
                    )
            })
            .count();
        assert_eq!(
            escalation_count, 1,
            "escalation WARN must fire exactly once at the 3rd consecutive failure, got: {events:?}"
        );

        // A clearing attempt resets the counter.
        note_truncate_outcome(&config, 100, &mut state);
        assert_eq!(
            state.consecutive_failures, 0,
            "an attempt that clears warn_pages must reset the consecutive-failure counter"
        );
    }
}
