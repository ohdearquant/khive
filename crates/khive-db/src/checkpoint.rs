//! Periodic WAL checkpoint task for the connection pool.
//!
//! Issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick — including when the
//! WAL page count exceeds the high-water mark. Ordinary ticks stay
//! PASSIVE-only and non-blocking; a rare, separately-gated escalation may
//! additionally run `PRAGMA wal_checkpoint(TRUNCATE)` under the same writer
//! guard with a shortened busy timeout — see the ADR-091 Plank 2 doc below
//! for the escalation's own gating.
//!
//! Non-contending design: `checkpoint_once` uses `try_writer_nowait` (zero-wait
//! `try_lock`) so a tick is skipped immediately when any writer holds the mutex,
//! rather than blocking for up to `checkout_timeout`. The checkpoint task must
//! never stall active write traffic — a skipped tick is always preferable.
//!
//! Why TRUNCATE is excluded from *every ordinary* tick: TRUNCATE inherits
//! RESTART semantics — it waits for active readers to release their WAL
//! snapshots and invokes the busy handler before acquiring the exclusive lock
//! needed to reset the WAL file. With PoolConfig's 30 s busy_timeout, blindly
//! running it every tick could sit inside SQLite holding the sole writer
//! connection for up to 30 s, stalling all normal write traffic. PASSIVE never
//! waits for readers; it checkpoints as many frames as currently possible and
//! returns promptly. When WAL pressure is sustained (high_water_pages
//! exceeded), the task emits a WARNING; once WAL pressure reaches the much
//! higher `truncate_high_water_pages` mark, the rare Plank 2 escalation below
//! may additionally attempt a bounded, rate-limited TRUNCATE under a
//! deliberately shortened busy timeout — replacing what used to be a purely
//! operator-scheduled manual step.
//!
//! Threshold-crossing WARN semantics: both the `warn_pages` and `high_water_pages`
//! warnings fire at most once per below→above crossing. Skipped ticks (writer
//! busy) leave the crossing state unchanged so that a skip cannot spuriously
//! re-arm the rate limit while WAL pressure is still elevated. The ADR-091
//! Plank 0 open-transaction-registry WARNs (oldest-entry escalation and the
//! high-water snapshot enumeration) ride the SAME crossing gates — they are
//! not independently rate-limited, so they never repeat on consecutive ticks
//! that remain above a threshold. Only the per-tick `debug!` trace of the
//! oldest open entry, and the ADR-091 Plank 1 age sweep below, run
//! unconditionally on every tick — including a Skipped one; the sweep's own
//! emissions stay edge-triggered per rung via `TxAgeSweepState`.
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
//! here — the tx_registry is only read for diagnostics; Plank 1 (below) owns
//! the registry's own bound.
//!
//! ADR-091 Plank 1: age-based background sweep, not per-statement rejection.
//! The ADR's original text describes a "cooperative stale-op guard" that
//! rejects further statements and rolls back a late `commit()` on a
//! `SqliteTransaction`/`begin_tx` span past `KHIVE_TX_MAX_AGE_SECS`. That API
//! no longer exists in this codebase: ADR-067's `atomic_unit` replaced every
//! production write-transaction path with a closure that structurally cannot
//! outlive its own call (single-poll-enforced on the write-queue path), which
//! is the ADR's own named follow-up ("closure-scoped transaction API"),
//! already delivered for writes by a later ADR. What ships here instead is
//! the part of Plank 1 that still applies to every registered span
//! regardless of which mechanism created it: on EVERY tick — Skipped as well
//! as Observed, since a registered `WriterGuard::transaction` span holds the
//! writer mutex for its whole lifetime and would otherwise make the busiest,
//! most relevant tick invisible to this sweep — `TxAgeSweepState` checks
//! `khive_storage::tx_registry::oldest()`'s age against
//! `tx_warn_secs`/`tx_max_age_secs` and escalates to `warn!`/`error!` on each
//! below→above crossing (same debounce idiom as the WAL-pressure ladder — a
//! sustained stale span logs once per rung, not once per tick), also
//! re-arming both rungs if the oldest entry's identity changes between ticks
//! so a departed span's latched state cannot suppress its replacement. This
//! is visibility, not reclamation: nothing here force-closes a stale span,
//! matching the ADR's own accepted gap for a transaction "held idle across
//! an await with no further calls."

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pool::{ConnectionPool, WriterGuard};

// ── metrics read-surface (load/perf harness) ─────────────────────────────
//
// Process-wide gauges mirroring the fallback-counter pattern in
// `khive-mcp/src/daemon.rs` (`FALLBACK_*` statics + their `pub(crate)`
// accessors): the checkpoint task is a single fire-and-forget
// `tokio::spawn` with no handle retained anywhere the daemon's
// connection-accept loop can reach, so these are plain module-scoped
// atomics rather than a struct threaded through every `checkpoint_once`/
// `maybe_truncate`/`note_truncate_outcome` call site (and, transitively,
// every existing test call site). Read-only surface: nothing here is ever
// reset outside `#[cfg(test)]`, and nothing reachable over the daemon wire
// can reset them either (see `khive_runtime::daemon::DaemonRequestFrame::
// metrics_only`).

/// Last-observed WAL page count (`query_wal_pages`'s return value on its
/// most recent call, from either `checkpoint_once` or `maybe_truncate`).
/// `u64::MAX` is the "never observed" sentinel — no checkpoint tick has run
/// yet in this process — distinct from a genuine zero-page WAL.
static LAST_WAL_PAGES: AtomicU64 = AtomicU64::new(u64::MAX);

/// Count of TRUNCATE attempts (`maybe_truncate`'s pragma actually invoked,
/// win or lose) across this process's lifetime.
static TRUNCATE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Current consecutive-failure count, mirrored from the caller-owned
/// `TruncateState::consecutive_failures` field into a process-readable
/// gauge every time `note_truncate_outcome` runs.
static TRUNCATE_CONSECUTIVE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Count of checkpoint ticks skipped because the writer mutex was already
/// held (ADR-091 checkpoint-pressure telemetry), across this process's
/// lifetime. Never reset outside `#[cfg(test)]`.
static CHECKPOINT_SKIPPED_TICKS: AtomicU64 = AtomicU64::new(0);

/// Current run-length of consecutive skipped ticks. Reset to 0 the next time
/// a tick is actually observed (writer free), so a sustained skip streak is
/// visible even between two successful observations.
static CHECKPOINT_CONSECUTIVE_SKIPS: AtomicU64 = AtomicU64::new(0);

/// WAL page count as of the most recent *observed* tick, snapshotted at the
/// moment a skip occurs. `u64::MAX` is the "no skip has recorded a snapshot
/// yet" sentinel, mirroring `LAST_WAL_PAGES`.
static CHECKPOINT_LAST_SKIP_WAL_PAGES: AtomicU64 = AtomicU64::new(u64::MAX);

/// Last-observed WAL page count, if any checkpoint tick has run yet in this
/// process. Read surface for the daemon-frame metrics snapshot.
pub fn last_observed_wal_pages() -> Option<u64> {
    match LAST_WAL_PAGES.load(Ordering::Relaxed) {
        u64::MAX => None,
        pages => Some(pages),
    }
}

/// Total WAL TRUNCATE attempts made in this process's lifetime.
pub fn truncate_attempts() -> u64 {
    TRUNCATE_ATTEMPTS.load(Ordering::Relaxed)
}

/// Current consecutive TRUNCATE-attempt failure count.
pub fn truncate_consecutive_failures() -> u64 {
    TRUNCATE_CONSECUTIVE_FAILURES.load(Ordering::Relaxed)
}

/// Total checkpoint ticks skipped (writer busy) in this process's lifetime.
pub fn checkpoint_skipped_ticks() -> u64 {
    CHECKPOINT_SKIPPED_TICKS.load(Ordering::Relaxed)
}

/// Current consecutive-skip run length; 0 once the next tick is observed.
pub fn checkpoint_consecutive_skips() -> u64 {
    CHECKPOINT_CONSECUTIVE_SKIPS.load(Ordering::Relaxed)
}

/// WAL page count last known at the time of the most recent skip, if any
/// skip has occurred yet in this process.
pub fn checkpoint_last_skip_wal_pages() -> Option<u64> {
    match CHECKPOINT_LAST_SKIP_WAL_PAGES.load(Ordering::Relaxed) {
        u64::MAX => None,
        pages => Some(pages),
    }
}

/// A tick's writer checkout was skipped (mutex busy): bump the lifetime and
/// consecutive-skip counters and snapshot the last-known WAL pressure so an
/// operator can see how bad the WAL was heading into the skip streak.
fn note_checkpoint_skipped() {
    CHECKPOINT_SKIPPED_TICKS.fetch_add(1, Ordering::Relaxed);
    CHECKPOINT_CONSECUTIVE_SKIPS.fetch_add(1, Ordering::Relaxed);
    if let Some(pages) = last_observed_wal_pages() {
        CHECKPOINT_LAST_SKIP_WAL_PAGES.store(pages, Ordering::Relaxed);
    }
}

/// A tick was actually observed (writer free): close out any prior skip
/// streak. `_wal_pages` is accepted for call-site symmetry with
/// `note_checkpoint_skipped` and to leave room for a future observed-side
/// gauge without changing this function's signature again.
fn note_checkpoint_observed(_wal_pages: u64) {
    CHECKPOINT_CONSECUTIVE_SKIPS.store(0, Ordering::Relaxed);
}

/// Reset the checkpoint-pressure atomics between tests. Process-wide gauges
/// are otherwise shared across every test in this binary; tests that assert
/// on them must reset first and run under a shared `#[serial(...)]` group.
#[cfg(test)]
pub(crate) fn reset_checkpoint_metrics_for_tests() {
    CHECKPOINT_SKIPPED_TICKS.store(0, Ordering::Relaxed);
    CHECKPOINT_CONSECUTIVE_SKIPS.store(0, Ordering::Relaxed);
    CHECKPOINT_LAST_SKIP_WAL_PAGES.store(u64::MAX, Ordering::Relaxed);
}

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

/// Default number of consecutive above-`warn_pages` observed ticks required
/// to escalate from the INFO to the WARN rung of the ADR-091 severity ladder.
pub const DEFAULT_WARN_SUSTAINED_CYCLES: u8 = 3;

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

    /// Number of consecutive observed ticks with `wal_pages >= warn_pages`
    /// required before the ADR-091 severity ladder escalates from INFO
    /// (first crossing) to WARN (sustained pressure). Edge-triggered once
    /// per elevation episode — see [`CheckpointSeverityState`].
    ///
    /// Overridable via `KHIVE_WAL_WARN_SUSTAINED_CYCLES`.
    /// Default: 3 cycles.
    pub warn_sustained_cycles: u8,

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

    /// ADR-091 Plank 1 soft cap: age past which the oldest entry in the
    /// shared open-transaction registry (`khive_storage::tx_registry`,
    /// covering every registered span regardless of which call site created
    /// it) is surfaced at `tracing::warn!` on the background sweep this
    /// module runs on EVERY tick — Skipped as well as Observed — independent
    /// of WAL page pressure: a registered span can go stale while
    /// `wal_pages` sits well under `warn_pages`, or while the writer is busy
    /// and no WAL page count is sampled at all this tick.
    ///
    /// Overridable via `KHIVE_TX_WARN_SECS`.
    /// Default: 30 seconds.
    pub tx_warn_secs: Duration,

    /// ADR-091 Plank 1 cooperative-stale-op-guard threshold: age past which
    /// the same sweep escalates the oldest registry entry to
    /// `tracing::error!`. This module has no per-statement hook back into a
    /// registered span's own caller to reject further statements or force a
    /// rollback the way the ADR's original text describes — that mechanism
    /// targeted `SqliteTransaction`/`begin_tx`, which ADR-067's `atomic_unit`
    /// closure API has since replaced for every production write path (a
    /// closure that structurally cannot outlive its own call is the ADR's
    /// own named follow-up, already delivered for writes by a later ADR).
    /// So past this age the sweep can only make a stale span maximally
    /// visible — not force-close it — exactly the accepted gap the ADR
    /// itself documents under Failure modes.
    ///
    /// Overridable via `KHIVE_TX_MAX_AGE_SECS`.
    /// Default: 120 seconds.
    pub tx_max_age_secs: Duration,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            warn_pages: 2000,
            warn_sustained_cycles: DEFAULT_WARN_SUSTAINED_CYCLES,
            high_water_pages: 6000,
            truncate_high_water_pages: 20_000,
            truncate_min_interval: Duration::from_secs(300),
            truncate_busy_timeout: Duration::from_millis(2000),
            tx_warn_secs: Duration::from_secs(30),
            tx_max_age_secs: Duration::from_secs(120),
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

        if let Ok(v) = std::env::var("KHIVE_WAL_WARN_SUSTAINED_CYCLES") {
            if let Ok(n) = v.parse::<u8>() {
                if n > 0 {
                    cfg.warn_sustained_cycles = n;
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

        if let Ok(v) = std::env::var("KHIVE_TX_WARN_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.tx_warn_secs = Duration::from_secs(n);
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_TX_MAX_AGE_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.tx_max_age_secs = Duration::from_secs(n);
                }
            }
        }

        // The severity ladder assumes tx_warn_secs < tx_max_age_secs (Warn
        // fires before Stale as an entry ages). A reversed or equal pair —
        // whether from one misconfigured var or the interaction of both —
        // would invert or collapse that ordering (e.g. WARN_SECS=120,
        // MAX_AGE_SECS=30 emits Stale at 30s and never reaches the Warn
        // crossing until 120s), so both are rejected together rather than
        // silently honored. Resetting both to their defaults (rather than
        // just clamping one) avoids guessing which of the two the operator
        // actually meant to change.
        if cfg.tx_warn_secs >= cfg.tx_max_age_secs {
            let default = Self::default();
            tracing::warn!(
                configured_tx_warn_secs = cfg.tx_warn_secs.as_secs_f64(),
                configured_tx_max_age_secs = cfg.tx_max_age_secs.as_secs_f64(),
                fallback_tx_warn_secs = default.tx_warn_secs.as_secs_f64(),
                fallback_tx_max_age_secs = default.tx_max_age_secs.as_secs_f64(),
                "KHIVE_TX_WARN_SECS must be strictly less than KHIVE_TX_MAX_AGE_SECS; \
                 both transaction-age thresholds were rejected and reset to their defaults"
            );
            cfg.tx_warn_secs = default.tx_warn_secs;
            cfg.tx_max_age_secs = default.tx_max_age_secs;
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

/// ADR-091 graduated severity rung for sustained WAL pressure.
///
/// `Alarm` is never produced by [`CheckpointSeverityState::observe_wal_pages`]
/// — it labels the existing TRUNCATE-escalation tier (`maybe_truncate`),
/// which is gated on its own threshold/interval state, not on this ladder.
/// It exists here so callers and tests can name all three rungs uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointSeverityRung {
    /// First observed tick crossing `warn_pages` after a below-warn tick.
    Info,
    /// `warn_sustained_cycles` consecutive observed ticks at/above
    /// `warn_pages`; edge-triggered once per elevation episode.
    Warn,
    /// The TRUNCATE-escalation tier (`checkpoint_high_water_pages` and
    /// above); never emitted by `observe_wal_pages`.
    Alarm,
}

/// ADR-091 severity ladder state, carried across ticks by the caller
/// alongside [`TruncateState`]. Pure state machine: no I/O, no logging —
/// callers turn the returned emissions into `tracing` calls.
#[derive(Debug, Default, Clone)]
pub struct CheckpointSeverityState {
    /// Whether the previous observed tick was at/above `warn_pages`. Drives
    /// the below→above edge that fires INFO.
    was_above_warn: bool,
    /// Run-length of consecutive observed ticks at/above `warn_pages` in the
    /// current elevation episode. Resets to 0 on any below-warn tick.
    consecutive_above_warn: u8,
    /// Whether WARN has already fired for the current elevation episode, so
    /// sustained pressure logs WARN once per episode, not once per tick past
    /// the threshold.
    warn_emitted_for_episode: bool,
}

/// One severity-ladder emission produced by a single
/// [`CheckpointSeverityState::observe_wal_pages`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointSeverityEmission {
    /// Which rung this emission represents (`Info` or `Warn`; see
    /// [`CheckpointSeverityRung::Alarm`] doc for why `Alarm` never appears
    /// here).
    pub rung: CheckpointSeverityRung,
    /// The WAL page count observed on the tick that produced this emission.
    pub wal_pages: u64,
    /// The `warn_pages` threshold in effect for this tick.
    pub threshold_pages: u64,
    /// Consecutive above-warn cycle count as of this tick (1 on the INFO
    /// edge, `warn_sustained_cycles` on the WARN edge).
    pub consecutive_cycles: u8,
}

impl CheckpointSeverityState {
    /// Advance the severity ladder by one observed tick and return every
    /// rung crossed on this tick (zero, one, or two emissions: a fresh
    /// elevation episode can produce INFO and, if `warn_sustained_cycles`
    /// is 1, WARN on the very same tick).
    ///
    /// A below-warn tick resets both the consecutive-cycle counter and the
    /// per-episode WARN latch, re-arming INFO/WARN for a later episode.
    /// Skipped ticks must not be passed here at all — the caller only calls
    /// this on `CheckpointTick::Observed`, matching the existing
    /// threshold-crossing WARN's skip-leaves-state-unchanged rule.
    pub fn observe_wal_pages(
        &mut self,
        wal_pages: u64,
        config: &CheckpointConfig,
    ) -> Vec<CheckpointSeverityEmission> {
        let mut emissions = Vec::new();
        let above_warn = wal_pages >= config.warn_pages;

        if above_warn {
            self.consecutive_above_warn = self.consecutive_above_warn.saturating_add(1);

            if !self.was_above_warn {
                emissions.push(CheckpointSeverityEmission {
                    rung: CheckpointSeverityRung::Info,
                    wal_pages,
                    threshold_pages: config.warn_pages,
                    consecutive_cycles: self.consecutive_above_warn,
                });
            }

            if !self.warn_emitted_for_episode
                && self.consecutive_above_warn >= config.warn_sustained_cycles
            {
                emissions.push(CheckpointSeverityEmission {
                    rung: CheckpointSeverityRung::Warn,
                    wal_pages,
                    threshold_pages: config.warn_pages,
                    consecutive_cycles: self.consecutive_above_warn,
                });
                self.warn_emitted_for_episode = true;
            }
        } else {
            self.consecutive_above_warn = 0;
            self.warn_emitted_for_episode = false;
        }

        self.was_above_warn = above_warn;
        emissions
    }
}

/// ADR-091 Plank 1 rung for the open-transaction registry's background age
/// sweep: independent of the WAL-pressure ladder above, keyed purely off how
/// long the registry's oldest entry has been open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxAgeRung {
    /// The oldest registry entry's age crossed `tx_warn_secs`.
    Warn,
    /// The oldest registry entry's age crossed `tx_max_age_secs` — the ADR's
    /// "cooperative stale-op guard" cap. No in-process mechanism force-closes
    /// it (see [`CheckpointConfig::tx_max_age_secs`]); this rung is the
    /// sweep's strongest available signal.
    Stale,
}

/// One emission produced by a single [`TxAgeSweepState::observe`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxAgeEmission {
    pub rung: TxAgeRung,
    pub age: Duration,
    pub label: Option<String>,
}

/// ADR-091 Plank 1 background-sweep state, carried across ticks by the
/// caller alongside [`CheckpointSeverityState`] and [`TruncateState`]. Pure
/// state machine: no I/O, no logging — callers turn the returned emissions
/// into `tracing` calls, mirroring [`CheckpointSeverityState`]'s shape.
///
/// Keyed off `khive_storage::tx_registry::oldest()` — the single oldest
/// entry across every registered span, regardless of which call site
/// (`atomic_unit`, `WriterGuard::transaction`, a store's own batch-upsert
/// helper, or `graph.rs`'s chunked-traversal read snapshot) created it. This
/// is deliberately a different signal from the WAL-pressure ladder: a
/// registered span can go stale while `wal_pages` sits well under
/// `warn_pages` (nothing has pinned the checkpoint boundary yet), and
/// conversely `wal_pages` can be elevated with an empty registry (the pin,
/// if any, is outside this process — see the ADR's own "route reads through
/// the daemon" alternative, out of scope here).
#[derive(Debug, Default, Clone)]
pub struct TxAgeSweepState {
    /// Whether the previous observed tick's oldest entry was at/above
    /// `tx_warn_secs`. Drives the below→above edge that fires `Warn`.
    was_above_warn: bool,
    /// Whether the previous observed tick's oldest entry was at/above
    /// `tx_max_age_secs`. Drives the below→above edge that fires `Stale`.
    was_above_max_age: bool,
    /// Identity of the entry the previous observed tick reported as oldest,
    /// or `None` if the registry was empty. Tracked separately from the two
    /// latches above so a change in *which span* is oldest can be detected
    /// even when both latches are already `true` (see [`Self::observe`]).
    tracked_id: Option<khive_storage::tx_registry::TxId>,
}

impl TxAgeSweepState {
    /// Advance by one observed tick given the registry's current oldest
    /// entry (identity, age, label), or `None` if the registry is empty.
    /// Returns zero, one, or two emissions: an entry can cross both rungs on
    /// the same tick if it was already stale the first time this sweep
    /// observed it in that identity — e.g. right after process start, or
    /// right after it became the new oldest entry by replacing a
    /// since-closed span.
    ///
    /// A below-threshold oldest entry (or no entry at all) resets both
    /// latches, so a future stale span re-arms both rungs. Identity is also
    /// tracked explicitly: if the oldest entry's [`TxId`](khive_storage::tx_registry::TxId)
    /// differs from the previous tick's, both latches are force-reset before
    /// re-evaluating the new entry's age, even though this happens on the
    /// SAME tick as the age check (not a separate below-threshold tick in
    /// between). Without this, an already-latched-`true` state from the
    /// *departed* entry would silently suppress the crossing for a
    /// *different* span that replaced it while already stale — naming
    /// nobody at exactly the moment a new long-lived span starts pinning the
    /// database, which is the scenario this sweep exists to catch. A merely
    /// fresher replacement still reads as below-threshold either way, so the
    /// identity check changes behavior only for an already-stale successor.
    pub fn observe(
        &mut self,
        oldest: Option<(khive_storage::tx_registry::TxId, Duration, Option<String>)>,
        config: &CheckpointConfig,
    ) -> Vec<TxAgeEmission> {
        let mut emissions = Vec::new();

        let Some((id, age, label)) = oldest else {
            self.was_above_warn = false;
            self.was_above_max_age = false;
            self.tracked_id = None;
            return emissions;
        };

        if self.tracked_id != Some(id) {
            self.was_above_warn = false;
            self.was_above_max_age = false;
        }
        self.tracked_id = Some(id);

        let above_warn = age >= config.tx_warn_secs;
        let above_max_age = age >= config.tx_max_age_secs;

        if above_warn && !self.was_above_warn {
            emissions.push(TxAgeEmission {
                rung: TxAgeRung::Warn,
                age,
                label: label.clone(),
            });
        }
        if above_max_age && !self.was_above_max_age {
            emissions.push(TxAgeEmission {
                rung: TxAgeRung::Stale,
                age,
                label,
            });
        }

        self.was_above_warn = above_warn;
        self.was_above_max_age = above_max_age;
        emissions
    }
}

/// ADR-091 Plank 1: turn a [`TxAgeEmission`] into the appropriate `tracing`
/// call. Extracted from `run_checkpoint_task` so tests can drive the same
/// logging path `CaptureSubscriber`-style without spinning up the async task
/// (mirrors [`log_tx_registry_oldest_warn`]/[`log_tx_registry_snapshot_warn`]).
fn log_tx_age_emission(emission: &TxAgeEmission) {
    let label = emission.label.as_deref().unwrap_or("<unlabeled>");
    match emission.rung {
        TxAgeRung::Warn => {
            tracing::warn!(
                tx_age_secs = emission.age.as_secs_f64(),
                tx_label = label,
                "ADR-091 Plank 1: open transaction registry entry exceeded soft-cap age"
            );
        }
        TxAgeRung::Stale => {
            tracing::error!(
                tx_age_secs = emission.age.as_secs_f64(),
                tx_label = label,
                "ADR-091 Plank 1: open transaction registry entry exceeded the cooperative \
                 stale-op cap; no in-process mechanism can force-close it — investigate the \
                 labeled caller directly"
            );
        }
    }
}

/// Run the WAL checkpoint background task.
///
/// This is a long-running async task that should be spawned with
/// `tokio::spawn`. It loops until `shutdown_rx` observes a change (or its
/// sender is dropped), at which point it exits on its next `select!` wakeup.
/// Callers should hold the paired `tokio::sync::watch::Sender` for the
/// daemon's run scope and send on it as part of the shutdown sequence.
///
/// An earlier version of this task used `Arc::strong_count(&pool) <= 1` as
/// its exit condition instead of an explicit signal. That check is
/// unreachable whenever a sibling owner holds its own clone of `pool` for
/// the task's lifetime — which the production boot path does: `event_store`
/// (`Option<Arc<dyn EventStore>>`), when `Some`, is a `SqlEventStore` that
/// retains its own `Arc::clone` of the same pool, so the task always
/// observed `strong_count == 2` and never exited via that mechanism
/// (issue #774). The explicit watch channel does not depend on how many
/// other owners exist.
///
/// The task issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick — ordinary
/// ticks stay PASSIVE-only and non-blocking; see the module-level doc for the
/// rare Plank 2 TRUNCATE escalation `checkpoint_once` may additionally run
/// under the same writer guard when WAL pressure is sustained past
/// `truncate_high_water_pages`. A WARNING is emitted once on threshold
/// crossing (wal_pages transitions from below a threshold to at/above) rather
/// than on every tick, preventing log spam when a long-lived reader pins a
/// WAL snapshot.
///
/// Skipped ticks (writer mutex busy) leave both crossing-state flags unchanged
/// so that a skip cannot spuriously re-arm the rate limit while WAL pressure is
/// still elevated.
///
/// Uses `try_writer_nowait` (zero-wait try-lock) so a busy writer causes the
/// current tick to be skipped rather than stalling write traffic.
///
/// `event_store` (ADR-094): when `Some`, this task appends a best-effort
/// `CheckpointOutcomeRecorded` lifecycle event on every tick where WAL
/// pressure is at/above `warn_pages`, plus exactly one drain row on the tick
/// that observes pressure fall back below `warn_pages` after an elevated
/// episode — never on every ordinary below-warn tick. `namespace` is
/// stamped on those rows. `None` makes event emission a pure no-op, exactly
/// like an unconfigured audit sink elsewhere in the runtime.
pub async fn run_checkpoint_task(
    pool: Arc<ConnectionPool>,
    config: CheckpointConfig,
    event_store: Option<Arc<dyn khive_storage::EventStore>>,
    namespace: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
) {
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut severity_state = CheckpointSeverityState::default();
    let mut tx_age_state = TxAgeSweepState::default();
    let mut was_above_high_water = false;
    let mut truncate_state = TruncateState::default();
    // Independent of `severity_state` (which owns the WARN-episode ladder
    // internally): this tracks only the "was the previous observed tick
    // elevated" edge the ADR-094 event emission needs, so the event path
    // never has to reach into the severity state machine's private fields.
    let mut event_was_elevated = false;

    loop {
        // A closed sender (the daemon returning without an explicit send)
        // makes `changed()` resolve with `Err` immediately, which `select!`
        // treats as ready — so shutdown is observed either way, not just on
        // an explicit send.
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.changed() => break,
        }

        let tick = checkpoint_once(&pool, &config, &mut truncate_state);

        // ADR-091 Plank 1: age-based sweep over the registry's oldest entry
        // MUST run on every tick, including a Skipped one — deliberately
        // BEFORE the Skipped early-continue below. A registered
        // `WriterGuard::transaction` span (`pool.rs`) holds the writer mutex
        // for its entire registered lifetime, so exactly the long-running
        // transaction this sweep exists to name is the one that makes an
        // ordinary checkpoint tick observe `Skipped` — gating the sweep on
        // `Observed` would silence it for precisely that scenario, defeating
        // the WAL-independent diagnostic the ADR specifies. Independent of
        // WAL page pressure by the same design: a registered span can go
        // stale (KHIVE_TX_WARN_SECS / KHIVE_TX_MAX_AGE_SECS) while
        // wal_pages sits well under warn_pages, or isn't sampled at all this
        // tick. Edge-triggered per rung, same debounce idiom as the severity
        // ladder below, so a sustained stale span logs once per rung rather
        // than once per tick.
        for emission in tx_age_state.observe(khive_storage::tx_registry::oldest(), &config) {
            log_tx_age_emission(&emission);
        }

        // Skipped ticks leave crossing state unchanged — a busy tick must not
        // re-arm the rate limit while WAL pressure is still elevated.
        let wal_pages = match tick {
            CheckpointTick::Skipped => continue,
            CheckpointTick::Observed(n) => n,
        };

        let above_warn = wal_pages >= config.warn_pages;
        let above_high_water = wal_pages >= config.high_water_pages;
        let above_truncate_high_water = wal_pages >= config.truncate_high_water_pages;

        // Per-tick debug for the oldest open entry always fires (cheap, single
        // `oldest()` lookup); the two `warn!`-level registry logs below are
        // gated on the SAME crossing state as the WAL-threshold WARNs above,
        // so sustained pressure logs once per crossing, not once per tick.
        log_tx_registry_oldest_debug(wal_pages);

        // ADR-091 severity ladder: INFO on the first below→above crossing,
        // WARN once `warn_sustained_cycles` consecutive ticks stay elevated.
        // The oldest-entry registry WARN rides the same INFO edge the old
        // binary crossing_warn used to gate on.
        for emission in severity_state.observe_wal_pages(wal_pages, &config) {
            match emission.rung {
                CheckpointSeverityRung::Info => {
                    log_tx_registry_oldest_warn(wal_pages);
                    tracing::info!(
                        wal_pages = emission.wal_pages,
                        warn_threshold = emission.threshold_pages,
                        "WAL page count crossed warn threshold"
                    );
                }
                CheckpointSeverityRung::Warn => {
                    tracing::warn!(
                        wal_pages = emission.wal_pages,
                        warn_threshold = emission.threshold_pages,
                        consecutive_cycles = emission.consecutive_cycles,
                        "WAL page count failed to drain below warn threshold"
                    );
                }
                CheckpointSeverityRung::Alarm => {
                    // Never produced by `observe_wal_pages`; see its doc.
                }
            }
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

        // ADR-094: emit every elevated tick, plus exactly one drain row on
        // the tick that observes the episode end — never on every ordinary
        // below-warn tick.
        if checkpoint_outcome_should_emit(above_warn, event_was_elevated) {
            let payload = khive_storage::CheckpointOutcomeRecordedPayload {
                wal_pages,
                warn_pages: config.warn_pages,
                high_water_pages: config.high_water_pages,
                truncate_high_water_pages: config.truncate_high_water_pages,
                above_warn,
                above_high_water,
                above_truncate_high_water,
            };
            append_checkpoint_lifecycle_event(
                event_store.as_ref(),
                &namespace,
                khive_types::EventKind::CheckpointOutcomeRecorded,
                payload,
            )
            .await;
        }
        event_was_elevated = above_warn;
    }
}

/// Whether a `CheckpointOutcomeRecorded` event should be emitted for this
/// tick: every elevated (`above_warn`) tick, plus exactly one drain row on
/// the first tick that observes a return to below-warn after an elevated
/// episode (`was_elevated`). An ordinary below-warn tick following another
/// below-warn tick emits nothing.
fn checkpoint_outcome_should_emit(above_warn: bool, was_elevated: bool) -> bool {
    above_warn || was_elevated
}

/// Append one ADR-094 lifecycle event on behalf of the checkpoint task.
///
/// Best-effort: `event_store == None` is a no-op, and an append failure is
/// logged and swallowed. No lifecycle-append error may ever interrupt or
/// slow down checkpoint/TRUNCATE work — the checkpoint task's correctness
/// does not depend on this succeeding.
async fn append_checkpoint_lifecycle_event<P: serde::Serialize>(
    store: Option<&Arc<dyn khive_storage::EventStore>>,
    namespace: &str,
    kind: khive_types::EventKind,
    payload: P,
) {
    let Some(store) = store else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_kind = %kind.name(),
                "failed to serialize checkpoint lifecycle event payload"
            );
            return;
        }
    };
    let event = khive_storage::Event::new(
        namespace,
        "checkpoint.lifecycle",
        kind,
        khive_types::SubstrateKind::Event,
        "daemon:checkpoint_task",
    )
    .with_payload(payload_value);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            event_kind = %kind.name(),
            "checkpoint lifecycle event append failed"
        );
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
    if let Some((_id, age, label)) = khive_storage::tx_registry::oldest() {
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
    if let Some((_id, age, label)) = khive_storage::tx_registry::oldest() {
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
        Err(_) => {
            note_checkpoint_skipped();
            return CheckpointTick::Skipped;
        }
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
/// `truncate_state.last_attempt` is stamped ONLY immediately before the
/// TRUNCATE pragma itself runs (writer held, threshold crossed, interval
/// elapsed, AND the temporary busy_timeout override successfully applied) —
/// every earlier return (below threshold, interval not elapsed, or the
/// busy_timeout override failing to apply) is a skip, not an attempt, and
/// never touches it, matching the ADR's "skip must not stamp" requirement.
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

    // Which caller (if any) is pinning the WAL — logged before the attempt so
    // it is available even if the attempt itself succeeds.
    log_tx_registry_snapshot_warn(wal_pages_before);

    let conn = writer.conn();
    let original_busy_timeout = pool.config().busy_timeout;

    if let Err(e) = conn.busy_timeout(config.truncate_busy_timeout) {
        // Setup failed before the TRUNCATE pragma ever ran — this is a skip,
        // not an attempt. `last_attempt` must NOT advance here (ADR-091
        // §377-382): stamping now would suppress the next eligible attempt
        // for the full `truncate_min_interval` on a path that never touched
        // the WAL at all.
        tracing::warn!(error = %e, "failed to lower busy_timeout for TRUNCATE attempt; skipping");
        return;
    }

    // Only now is this a genuine attempt: the writer is held, the threshold
    // and interval gates passed, and the busy_timeout override is in effect
    // immediately before the TRUNCATE pragma itself.
    truncate_state.last_attempt = Some(Instant::now());

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
    // Metrics read-surface (load/perf harness): this function runs exactly
    // once per genuine TRUNCATE attempt (both the `Ok` and `Err` outcome
    // arms in `maybe_truncate` call it once each), so incrementing here
    // counts total attempts without a separate call site.
    TRUNCATE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);

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

    TRUNCATE_CONSECUTIVE_FAILURES.store(state.consecutive_failures as u64, Ordering::Relaxed);
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
    let pages = conn
        .query_row("PRAGMA wal_checkpoint", [], |row| row.get::<_, i64>(1))
        .unwrap_or(0)
        .max(0) as u64;
    // Metrics read-surface (load/perf harness): mirror every observation into
    // the process-wide gauge, regardless of which caller (`checkpoint_once`
    // or `maybe_truncate`) triggered it.
    LAST_WAL_PAGES.store(pages, Ordering::Relaxed);
    note_checkpoint_observed(pages);
    pages
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
            .and_then(|(_, _, label)| label)
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

    /// ADR-091 Plank 1: `log_tx_age_emission` emits the correct message text
    /// and carries the entry's label, for both the `Warn` and `Stale` rungs.
    #[test]
    fn log_tx_age_emission_carries_label_for_both_rungs() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        tracing::subscriber::with_default(subscriber, || {
            log_tx_age_emission(&TxAgeEmission {
                rung: TxAgeRung::Warn,
                age: Duration::from_secs(45),
                label: Some("plank1_warn_test".to_string()),
            });
            log_tx_age_emission(&TxAgeEmission {
                rung: TxAgeRung::Stale,
                age: Duration::from_secs(150),
                label: Some("plank1_stale_test".to_string()),
            });
        });

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|e| {
                e.message.as_deref()
                    == Some(
                        "ADR-091 Plank 1: open transaction registry entry exceeded soft-cap age",
                    )
                    && e.tx_label.as_deref() == Some("plank1_warn_test")
            }),
            "expected a Warn-rung log line naming the entry, got: {events:?}"
        );
        assert!(
            events.iter().any(|e| {
                e.message.as_deref().is_some_and(|m| {
                    m.starts_with(
                        "ADR-091 Plank 1: open transaction registry entry exceeded the cooperative",
                    )
                }) && e.tx_label.as_deref() == Some("plank1_stale_test")
            }),
            "expected a Stale-rung log line naming the entry, got: {events:?}"
        );
    }

    fn file_pool(path: &std::path::Path) -> Arc<ConnectionPool> {
        let cfg = PoolConfig {
            path: Some(path.to_path_buf()),
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(cfg).expect("pool open"))
    }

    // `checkpoint_once` -> `query_wal_pages` writes the process-wide
    // `LAST_WAL_PAGES` gauge and resets `CHECKPOINT_CONSECUTIVE_SKIPS`
    // (see the reset-discipline comment on `reset_checkpoint_metrics_for_tests`
    // above) — this must join the `checkpoint_skip_metrics` group so it can
    // never interleave with a test asserting on those same gauges.
    #[test]
    #[serial(checkpoint_skip_metrics)]
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
    #[serial(checkpoint_skip_metrics)]
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
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_exits_on_shutdown_signal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal_task_shutdown.db");
        let pool = file_pool(&path);

        // Use a very short interval so the task ticks quickly in the test.
        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            ..Default::default()
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            None,
            "local".to_string(),
            shutdown_rx,
        ));

        shutdown_tx.send(()).expect("send shutdown signal");

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");
    }

    /// Regression for issue #774: on the production boot path, the daemon
    /// passes `run_checkpoint_task` both `pool` directly and an
    /// `event_store` that internally retains its own `Arc::clone` of the
    /// same pool (`SqlEventStore::new_scoped`). A strong-count-based exit
    /// condition can never fire in that shape, because the task always
    /// observes at least two live clones — its own `pool` argument plus the
    /// one buried in `event_store`. This test reproduces that exact
    /// ownership shape (a real `SqlEventStore` holding a sibling clone) and
    /// asserts the task still exits promptly via the watch-channel signal,
    /// proving the fix does not depend on `Arc::strong_count` at all.
    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_exits_via_shutdown_signal_with_live_event_store_pool_clone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal_task_event_store.db");
        let pool = file_pool(&path);

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            ..Default::default()
        };

        let event_store: Arc<dyn khive_storage::EventStore> =
            Arc::new(crate::stores::event::SqlEventStore::new_scoped(
                Arc::clone(&pool),
                true,
                "local".to_string(),
            ));
        // A second, independent sibling clone of `pool` outlives this test
        // function's own binding — mirrors `StorageBackend` retaining
        // `self.pool` alongside the `SqlEventStore` it hands to the
        // checkpoint task in production.
        let sibling_pool_clone = Arc::clone(&pool);

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            Some(event_store),
            "local".to_string(),
            shutdown_rx,
        ));

        // Confirm strong_count is well above 1 — the old check would spin
        // forever here — before proving the new signal-based exit works
        // regardless.
        assert!(
            Arc::strong_count(&sibling_pool_clone) > 1,
            "test setup must reproduce the multi-owner shape the bug depends on"
        );

        shutdown_tx.send(()).expect("send shutdown signal");

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect(
                "checkpoint task should exit within 1s via the watch signal, \
                 even with a live sibling Arc<ConnectionPool> clone held by \
                 the event store",
            )
            .expect("checkpoint task panicked");
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
        std::env::set_var("KHIVE_TX_WARN_SECS", "15");
        std::env::set_var("KHIVE_TX_MAX_AGE_SECS", "90");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");
        std::env::remove_var("KHIVE_TX_WARN_SECS");
        std::env::remove_var("KHIVE_TX_MAX_AGE_SECS");

        assert_eq!(cfg.interval, Duration::from_millis(250));
        assert_eq!(cfg.warn_pages, 1500);
        assert_eq!(cfg.high_water_pages, 8000);
        assert_eq!(cfg.truncate_high_water_pages, 12000);
        assert_eq!(cfg.truncate_min_interval, Duration::from_secs(60));
        assert_eq!(cfg.truncate_busy_timeout, Duration::from_millis(500));
        assert_eq!(cfg.tx_warn_secs, Duration::from_secs(15));
        assert_eq!(cfg.tx_max_age_secs, Duration::from_secs(90));
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
        std::env::set_var("KHIVE_TX_WARN_SECS", "not_a_number");
        std::env::set_var("KHIVE_TX_MAX_AGE_SECS", "0");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");
        std::env::remove_var("KHIVE_TX_WARN_SECS");
        std::env::remove_var("KHIVE_TX_MAX_AGE_SECS");

        assert_eq!(cfg.interval, default.interval);
        assert_eq!(cfg.warn_pages, default.warn_pages);
        assert_eq!(cfg.high_water_pages, default.high_water_pages);
        assert_eq!(
            cfg.truncate_high_water_pages,
            default.truncate_high_water_pages
        );
        assert_eq!(cfg.truncate_min_interval, default.truncate_min_interval);
        assert_eq!(cfg.truncate_busy_timeout, default.truncate_busy_timeout);
        assert_eq!(cfg.tx_warn_secs, default.tx_warn_secs);
        assert_eq!(cfg.tx_max_age_secs, default.tx_max_age_secs);
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
    #[serial(checkpoint_skip_metrics)]
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
        std::env::set_var("KHIVE_TX_WARN_SECS", "0");
        std::env::set_var("KHIVE_TX_MAX_AGE_SECS", "0");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_CHECKPOINT_INTERVAL_MS");
        std::env::remove_var("KHIVE_WAL_WARN_PAGES");
        std::env::remove_var("KHIVE_WAL_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS");
        std::env::remove_var("KHIVE_WAL_TRUNCATE_BUSY_MS");
        std::env::remove_var("KHIVE_TX_WARN_SECS");
        std::env::remove_var("KHIVE_TX_MAX_AGE_SECS");

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
        assert_eq!(
            cfg.tx_warn_secs, default.tx_warn_secs,
            "zero tx_warn_secs must fall back to default"
        );
        assert_eq!(
            cfg.tx_max_age_secs, default.tx_max_age_secs,
            "zero tx_max_age_secs must fall back to default"
        );
    }

    /// Round-2 fix (Medium finding 1): a reversed pair — `KHIVE_TX_WARN_SECS`
    /// >= `KHIVE_TX_MAX_AGE_SECS` — must not be honored independently. Before
    /// this fix, WARN_SECS=120 / MAX_AGE_SECS=30 parsed both values
    /// successfully (each is independently positive) and produced a sweep
    /// that emits `Stale` at 30s while never reaching the `Warn` crossing
    /// until 120s — inverting the intended severity ladder. Both thresholds
    /// must instead fall back to their defaults together.
    #[test]
    #[serial]
    fn checkpoint_config_rejects_reversed_tx_thresholds() {
        let default = CheckpointConfig::default();
        std::env::set_var("KHIVE_TX_WARN_SECS", "120");
        std::env::set_var("KHIVE_TX_MAX_AGE_SECS", "30");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_TX_WARN_SECS");
        std::env::remove_var("KHIVE_TX_MAX_AGE_SECS");

        assert_eq!(
            cfg.tx_warn_secs, default.tx_warn_secs,
            "a reversed pair must fall back tx_warn_secs to its default, got: {:?}",
            cfg.tx_warn_secs
        );
        assert_eq!(
            cfg.tx_max_age_secs, default.tx_max_age_secs,
            "a reversed pair must fall back tx_max_age_secs to its default, got: {:?}",
            cfg.tx_max_age_secs
        );
    }

    /// Same invariant, the degenerate equal case: WARN_SECS == MAX_AGE_SECS
    /// would make an entry cross both rungs on the exact same tick every
    /// time, collapsing the two-rung severity ladder into one. Must also
    /// fall back to defaults, not merely reject a strictly-reversed pair.
    #[test]
    #[serial]
    fn checkpoint_config_rejects_equal_tx_thresholds() {
        let default = CheckpointConfig::default();
        std::env::set_var("KHIVE_TX_WARN_SECS", "60");
        std::env::set_var("KHIVE_TX_MAX_AGE_SECS", "60");

        let cfg = CheckpointConfig::from_env();

        std::env::remove_var("KHIVE_TX_WARN_SECS");
        std::env::remove_var("KHIVE_TX_MAX_AGE_SECS");

        assert_eq!(
            cfg.tx_warn_secs, default.tx_warn_secs,
            "an equal pair must fall back tx_warn_secs to its default, got: {:?}",
            cfg.tx_warn_secs
        );
        assert_eq!(
            cfg.tx_max_age_secs, default.tx_max_age_secs,
            "an equal pair must fall back tx_max_age_secs to its default, got: {:?}",
            cfg.tx_max_age_secs
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
    #[serial(tx_registry, checkpoint_skip_metrics)]
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
    #[serial(tx_registry, checkpoint_skip_metrics)]
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
    #[serial(tx_registry, checkpoint_skip_metrics)]
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
    /// per tick). Also asserts #646 checkpoint-pressure telemetry: a skipped
    /// tick must bump the skipped/consecutive-skip counters and snapshot the
    /// last-known WAL pressure.
    #[test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    fn busy_writer_skips_both_passive_and_truncate() {
        reset_checkpoint_metrics_for_tests();

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

        // An observed tick first, so the skip below has a last-known WAL
        // pressure snapshot to carry forward.
        let mut warmup_state = TruncateState::default();
        let warmup_tick = checkpoint_once(&pool, &CheckpointConfig::default(), &mut warmup_state);
        let observed_pages = match warmup_tick {
            CheckpointTick::Observed(n) => n,
            CheckpointTick::Skipped => panic!("warmup tick must observe, not skip"),
        };
        assert_eq!(
            checkpoint_consecutive_skips(),
            0,
            "an observed tick must not itself count as a skip"
        );

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

        assert_eq!(
            checkpoint_skipped_ticks(),
            1,
            "one skipped tick must bump the lifetime skipped-tick counter"
        );
        assert_eq!(
            checkpoint_consecutive_skips(),
            1,
            "one skipped tick must bump the consecutive-skip run length"
        );
        assert_eq!(
            checkpoint_last_skip_wal_pages(),
            Some(observed_pages),
            "the skip must snapshot the last-observed WAL pressure"
        );
    }

    /// Observation branch: a checkpoint tick that is actually observed (writer
    /// free) must close out a prior skip streak, resetting the
    /// consecutive-skip counter to 0 without touching the lifetime total.
    #[test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    fn observed_tick_resets_consecutive_skips_but_not_lifetime_total() {
        reset_checkpoint_metrics_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skip_then_observe.db");
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

        // Two consecutive skipped ticks.
        {
            let _held = pool.try_writer().unwrap();
            let mut state = TruncateState::default();
            for _ in 0..2 {
                let tick = checkpoint_once(&pool, &CheckpointConfig::default(), &mut state);
                assert_eq!(tick, CheckpointTick::Skipped);
            }
        }
        assert_eq!(checkpoint_skipped_ticks(), 2);
        assert_eq!(checkpoint_consecutive_skips(), 2);

        // Now the writer is free: an observed tick must reset the streak.
        let mut state = TruncateState::default();
        let tick = checkpoint_once(&pool, &CheckpointConfig::default(), &mut state);
        assert!(matches!(tick, CheckpointTick::Observed(_)));

        assert_eq!(
            checkpoint_skipped_ticks(),
            2,
            "an observed tick must not change the lifetime skipped-tick total"
        );
        assert_eq!(
            checkpoint_consecutive_skips(),
            0,
            "an observed tick must reset the consecutive-skip run length"
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

    // ADR-091 #617: graduated severity ladder state-machine tests.

    fn severity_test_config() -> CheckpointConfig {
        CheckpointConfig {
            warn_pages: 100,
            warn_sustained_cycles: 3,
            ..CheckpointConfig::default()
        }
    }

    /// INFO rung: a below→above crossing emits exactly one INFO and no WARN
    /// (default `warn_sustained_cycles = 3`, only one above-warn tick here).
    #[test]
    fn severity_ladder_info_on_first_crossing_no_warn() {
        let config = severity_test_config();
        let mut state = CheckpointSeverityState::default();

        let below = state.observe_wal_pages(10, &config);
        assert!(below.is_empty(), "below-warn tick must emit nothing");

        let above = state.observe_wal_pages(150, &config);
        assert_eq!(
            above,
            vec![CheckpointSeverityEmission {
                rung: CheckpointSeverityRung::Info,
                wal_pages: 150,
                threshold_pages: 100,
                consecutive_cycles: 1,
            }],
            "first below->above crossing must emit exactly one INFO and no WARN"
        );
    }

    /// WARN rung: `warn_sustained_cycles` (3) consecutive above-warn ticks
    /// emit WARN exactly on the third tick, not before and not repeated after.
    #[test]
    fn severity_ladder_warn_on_third_consecutive_cycle() {
        let config = severity_test_config();
        let mut state = CheckpointSeverityState::default();

        let tick1 = state.observe_wal_pages(150, &config);
        assert_eq!(tick1.len(), 1);
        assert_eq!(tick1[0].rung, CheckpointSeverityRung::Info);

        let tick2 = state.observe_wal_pages(150, &config);
        assert!(
            tick2.is_empty(),
            "second consecutive above-warn tick must emit nothing yet"
        );

        let tick3 = state.observe_wal_pages(150, &config);
        assert_eq!(
            tick3,
            vec![CheckpointSeverityEmission {
                rung: CheckpointSeverityRung::Warn,
                wal_pages: 150,
                threshold_pages: 100,
                consecutive_cycles: 3,
            }],
            "WARN must fire exactly on the third consecutive above-warn tick"
        );

        let tick4 = state.observe_wal_pages(150, &config);
        assert!(
            tick4.is_empty(),
            "WARN must not repeat on a fourth consecutive above-warn tick"
        );
    }

    /// Re-arm: after a WARN episode drains below warn_pages, a fresh episode
    /// of `warn_sustained_cycles` above-warn ticks must WARN again.
    #[test]
    fn severity_ladder_rearms_warn_after_drain() {
        let config = severity_test_config();
        let mut state = CheckpointSeverityState::default();

        // First episode reaches WARN.
        for _ in 0..3 {
            state.observe_wal_pages(150, &config);
        }
        assert!(state.warn_emitted_for_episode);

        // Drain below warn_pages: resets the episode.
        let drain = state.observe_wal_pages(10, &config);
        assert!(drain.is_empty(), "a draining tick must emit nothing");

        // Second episode: INFO on first tick, no WARN until the third again.
        let reentry = state.observe_wal_pages(150, &config);
        assert_eq!(reentry.len(), 1);
        assert_eq!(reentry[0].rung, CheckpointSeverityRung::Info);

        let mid = state.observe_wal_pages(150, &config);
        assert!(mid.is_empty());

        let second_warn = state.observe_wal_pages(150, &config);
        assert_eq!(
            second_warn,
            vec![CheckpointSeverityEmission {
                rung: CheckpointSeverityRung::Warn,
                wal_pages: 150,
                threshold_pages: 100,
                consecutive_cycles: 3,
            }],
            "a fresh elevation episode after a drain must WARN again"
        );
    }

    /// False-positive guard: three isolated single-tick crossings, each
    /// followed by a drain, must never reach WARN — only INFO fires each time.
    #[test]
    fn severity_ladder_isolated_crossings_never_warn() {
        let config = severity_test_config();
        let mut state = CheckpointSeverityState::default();

        for _ in 0..3 {
            let crossing = state.observe_wal_pages(150, &config);
            assert_eq!(
                crossing.len(),
                1,
                "each isolated crossing must emit exactly one INFO"
            );
            assert_eq!(crossing[0].rung, CheckpointSeverityRung::Info);

            let drain = state.observe_wal_pages(10, &config);
            assert!(drain.is_empty(), "the drain tick must emit nothing");
        }

        assert!(
            !state.warn_emitted_for_episode,
            "isolated single-tick crossings must never accumulate into a WARN"
        );
    }

    /// ALARM rung: the existing TRUNCATE-attempt gate is the ADR-091 ALARM
    /// tier. `observe_wal_pages` never produces it; this test documents and
    /// locks in that boundary so a future change can't silently reroute
    /// ALARM through the INFO/WARN ladder.
    #[test]
    fn severity_ladder_never_emits_alarm() {
        let config = CheckpointConfig {
            warn_pages: 100,
            warn_sustained_cycles: 1,
            ..CheckpointConfig::default()
        };
        let mut state = CheckpointSeverityState::default();

        for wal_pages in [150, 200, 250, u64::MAX] {
            let emissions = state.observe_wal_pages(wal_pages, &config);
            assert!(
                emissions
                    .iter()
                    .all(|e| e.rung != CheckpointSeverityRung::Alarm),
                "observe_wal_pages must never emit the ALARM rung, got: {emissions:?}"
            );
        }
    }

    // ADR-091 Plank 1: `TxAgeSweepState` background-sweep state-machine tests.
    // Pure unit tests mirroring the severity-ladder tests above — no I/O.

    fn tx_age_test_config() -> CheckpointConfig {
        CheckpointConfig {
            tx_warn_secs: Duration::from_secs(30),
            tx_max_age_secs: Duration::from_secs(120),
            ..CheckpointConfig::default()
        }
    }

    /// Synthetic identity for `TxAgeSweepState::observe`'s pure unit tests
    /// below, which exercise identity-change detection without paying for a
    /// real `tx_registry::register` call. `TxId`'s wrapped value is public
    /// exactly to support this (see its doc comment in `khive-storage`).
    fn tx_id(n: u64) -> khive_storage::tx_registry::TxId {
        khive_storage::tx_registry::TxId(n)
    }

    /// No open entry: nothing fires, and any prior latch state clears.
    #[test]
    fn tx_age_sweep_empty_registry_emits_nothing() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        let emissions = state.observe(None, &config);
        assert!(emissions.is_empty(), "no open entry must emit nothing");
    }

    /// A fresh entry (age below both thresholds) emits nothing.
    #[test]
    fn tx_age_sweep_fresh_entry_emits_nothing() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        let emissions = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(5),
                Some("fresh_span".to_string()),
            )),
            &config,
        );
        assert!(emissions.is_empty(), "a fresh entry must emit nothing");
    }

    /// Below→above crossing of `tx_warn_secs` fires exactly one `Warn`
    /// emission carrying the entry's label; it must not repeat on a second
    /// tick that is still above `tx_warn_secs` but below `tx_max_age_secs`.
    #[test]
    fn tx_age_sweep_warn_fires_once_on_crossing() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        let tick1 = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(45),
                Some("stale_span".to_string()),
            )),
            &config,
        );
        assert_eq!(
            tick1,
            vec![TxAgeEmission {
                rung: TxAgeRung::Warn,
                age: Duration::from_secs(45),
                label: Some("stale_span".to_string()),
            }],
            "crossing tx_warn_secs must emit exactly one Warn"
        );

        let tick2 = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(50),
                Some("stale_span".to_string()),
            )),
            &config,
        );
        assert!(
            tick2.is_empty(),
            "Warn must not repeat while the entry stays in the warn band"
        );
    }

    /// Crossing `tx_max_age_secs` fires `Stale`; a further tick still above
    /// the cap must not repeat it.
    #[test]
    fn tx_age_sweep_stale_fires_once_on_crossing() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        // Drive through the warn crossing first, matching real elapsed-time
        // progression (an entry ages through the warn band before the max).
        state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(45),
                Some("stuck_writer_task_tx".to_string()),
            )),
            &config,
        );

        let tick = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(130),
                Some("stuck_writer_task_tx".to_string()),
            )),
            &config,
        );
        assert_eq!(
            tick,
            vec![TxAgeEmission {
                rung: TxAgeRung::Stale,
                age: Duration::from_secs(130),
                label: Some("stuck_writer_task_tx".to_string()),
            }],
            "crossing tx_max_age_secs must emit exactly one Stale"
        );

        let tick_repeat = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(200),
                Some("stuck_writer_task_tx".to_string()),
            )),
            &config,
        );
        assert!(
            tick_repeat.is_empty(),
            "Stale must not repeat while the entry stays above tx_max_age_secs"
        );
    }

    /// An entry already stale the first time the sweep observes it (e.g.
    /// right after process start with a pre-existing registry entry) crosses
    /// both rungs on the same tick.
    #[test]
    fn tx_age_sweep_already_stale_entry_emits_both_rungs_same_tick() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        let tick = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(300),
                Some("ancient_tx".to_string()),
            )),
            &config,
        );
        assert_eq!(
            tick,
            vec![
                TxAgeEmission {
                    rung: TxAgeRung::Warn,
                    age: Duration::from_secs(300),
                    label: Some("ancient_tx".to_string()),
                },
                TxAgeEmission {
                    rung: TxAgeRung::Stale,
                    age: Duration::from_secs(300),
                    label: Some("ancient_tx".to_string()),
                },
            ],
            "an already-stale entry must cross both rungs on its first observed tick"
        );
    }

    /// Re-arm: once the stale entry closes (registry reports a fresher
    /// oldest entry, or none at all), a future stale span must fire again.
    #[test]
    fn tx_age_sweep_rearms_after_entry_clears() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(150),
                Some("first_span".to_string()),
            )),
            &config,
        );

        // The stale span closed; nothing is open now.
        let cleared = state.observe(None, &config);
        assert!(cleared.is_empty(), "a clearing tick must emit nothing");

        // A fresh entry (unrelated span) is now oldest — still below threshold.
        let fresh = state.observe(
            Some((
                tx_id(2),
                Duration::from_secs(2),
                Some("second_span".to_string()),
            )),
            &config,
        );
        assert!(fresh.is_empty(), "a fresh oldest entry must emit nothing");

        // That second span goes stale in turn — must WARN again (re-armed).
        let rewarn = state.observe(
            Some((
                tx_id(2),
                Duration::from_secs(35),
                Some("second_span".to_string()),
            )),
            &config,
        );
        assert_eq!(
            rewarn,
            vec![TxAgeEmission {
                rung: TxAgeRung::Warn,
                age: Duration::from_secs(35),
                label: Some("second_span".to_string()),
            }],
            "a fresh stale episode after a clear must Warn again"
        );
    }

    /// Round-2 fix (Medium finding 2): a stale entry (A) that closes and is
    /// immediately replaced by an ALREADY-stale entry (B) on the very next
    /// observed tick — no intervening below-threshold or empty tick, unlike
    /// `tx_age_sweep_rearms_after_entry_clears` above — must still emit both
    /// rungs for B. Before the identity-tracking fix, `was_above_warn` and
    /// `was_above_max_age` were already `true` from A, so B's crossing was
    /// silently swallowed: the alert stayed latched to a departed caller
    /// while a different long-lived span was now pinning the database.
    #[test]
    fn tx_age_sweep_stale_replacement_without_intervening_clear_still_names_new_entry() {
        let config = tx_age_test_config();
        let mut state = TxAgeSweepState::default();

        let tick_a = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(300),
                Some("stale_entry_a".to_string()),
            )),
            &config,
        );
        assert_eq!(
            tick_a.len(),
            2,
            "entry A must cross both rungs on its first observed tick, got: {tick_a:?}"
        );

        // B replaces A as the oldest entry on the VERY NEXT tick — already
        // stale itself, with no intervening None/below-threshold tick.
        let tick_b = state.observe(
            Some((
                tx_id(2),
                Duration::from_secs(400),
                Some("stale_entry_b".to_string()),
            )),
            &config,
        );
        assert_eq!(
            tick_b,
            vec![
                TxAgeEmission {
                    rung: TxAgeRung::Warn,
                    age: Duration::from_secs(400),
                    label: Some("stale_entry_b".to_string()),
                },
                TxAgeEmission {
                    rung: TxAgeRung::Stale,
                    age: Duration::from_secs(400),
                    label: Some("stale_entry_b".to_string()),
                },
            ],
            "a same-tick identity change to an already-stale successor must re-emit both \
             rungs naming the NEW entry, got: {tick_b:?}"
        );
    }

    /// `KHIVE_TX_WARN_SECS` / `KHIVE_TX_MAX_AGE_SECS` are read into the
    /// config `from_env` reads at `run_checkpoint_task` construction time,
    /// so this closes the loop from env var to the actual emitted rung
    /// (the earlier `checkpoint_config_env_override` test only asserts the
    /// config fields themselves).
    #[test]
    fn tx_age_sweep_uses_configured_thresholds_not_hardcoded_defaults() {
        let config = CheckpointConfig {
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(2),
            ..CheckpointConfig::default()
        };
        let mut state = TxAgeSweepState::default();

        let tick = state.observe(
            Some((
                tx_id(1),
                Duration::from_millis(5),
                Some("fast_cap_span".to_string()),
            )),
            &config,
        );
        assert_eq!(
            tick.len(),
            2,
            "a millisecond-scale cap must cross both rungs immediately, got: {tick:?}"
        );
    }

    /// Integration-level regression for the incident this ADR fixes: a real
    /// `BEGIN DEFERRED` reader pins a WAL snapshot (exactly like
    /// `checkpoint_high_water_does_not_block_behind_reader` above) while also
    /// being registered in the shared `tx_registry` (simulating an
    /// instrumented long-lived-reader call site such as
    /// `graph.rs`'s `graph_traverse_read`), writes drive `wal_pages` past
    /// `high_water_pages`, and — with a millisecond-scale `tx_max_age_secs`
    /// so the test does not sleep for real minutes — the Plank 1 sweep
    /// escalates to `Stale` naming that exact reader, alongside the existing
    /// Plank 0 high-water WARN. This is the "detection works, mitigation
    /// missing" gap from the incident: the sweep now gives the operator the
    /// specific, escalating, un-silenced signal that a single one-shot
    /// high-water WARN does not.
    #[test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    fn tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx_age_sweep_reader_pin.db");
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

        // Open a real read transaction so it holds a WAL snapshot (same
        // isomorphism as `checkpoint_high_water_does_not_block_behind_reader`),
        // AND register it in tx_registry — the telemetry a real long-lived
        // reader call site (e.g. `graph_traverse_read`) is expected to carry.
        let reader = pool.reader().expect("reader");
        reader
            .execute_batch("BEGIN DEFERRED; SELECT * FROM t;")
            .expect("begin read tx");
        let _tx_handle =
            khive_storage::tx_registry::register(Some("tx_age_sweep_reader_pin_test".to_string()));

        // Drive writes past high_water_pages while the reader snapshot pins
        // the WAL tail — PASSIVE cannot reclaim these frames.
        let config = CheckpointConfig {
            high_water_pages: 1,
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(1),
            ..CheckpointConfig::default()
        };
        {
            let writer = pool.try_writer().unwrap();
            for i in 0..50 {
                writer
                    .conn()
                    .execute_batch(&format!("INSERT INTO t VALUES ({i});"))
                    .unwrap();
            }
        }

        let tick = checkpoint_once(&pool, &config, &mut TruncateState::default());
        let wal_pages = match tick {
            CheckpointTick::Observed(n) => n,
            CheckpointTick::Skipped => panic!("writer must not be busy in this test"),
        };
        assert!(
            wal_pages >= config.high_water_pages,
            "test setup must actually drive wal_pages ({wal_pages}) past high_water_pages \
             ({}) for this regression to mean anything",
            config.high_water_pages
        );

        // The Plank 1 sweep, given the SAME registry state, must name the
        // pinning reader at the Stale rung (millisecond-scale caps mean the
        // freshly-registered handle is already "stale" by the time we
        // observe it here, exactly like the always-stale-on-first-tick case
        // covered by the pure unit test above).
        let mut tx_age_state = TxAgeSweepState::default();
        let emissions = tx_age_state.observe(khive_storage::tx_registry::oldest(), &config);
        assert!(
            emissions.iter().any(|e| e.rung == TxAgeRung::Stale
                && e.label.as_deref() == Some("tx_age_sweep_reader_pin_test")),
            "expected a Stale emission naming the pinning reader, got: {emissions:?}"
        );

        reader.execute_batch("COMMIT;").ok();
        drop(reader);
        drop(_tx_handle);
    }

    /// `KHIVE_WAL_WARN_SUSTAINED_CYCLES` overrides the default and rejects 0.
    #[test]
    #[serial]
    fn checkpoint_config_warn_sustained_cycles_env_override() {
        let default = CheckpointConfig::default();
        assert_eq!(default.warn_sustained_cycles, DEFAULT_WARN_SUSTAINED_CYCLES);

        std::env::set_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES", "5");
        let cfg = CheckpointConfig::from_env();
        std::env::remove_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES");
        assert_eq!(cfg.warn_sustained_cycles, 5);

        std::env::set_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES", "0");
        let cfg_zero = CheckpointConfig::from_env();
        std::env::remove_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES");
        assert_eq!(
            cfg_zero.warn_sustained_cycles, DEFAULT_WARN_SUSTAINED_CYCLES,
            "zero must fall back to the default"
        );

        std::env::set_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES", "not_a_number");
        let cfg_invalid = CheckpointConfig::from_env();
        std::env::remove_var("KHIVE_WAL_WARN_SUSTAINED_CYCLES");
        assert_eq!(
            cfg_invalid.warn_sustained_cycles,
            DEFAULT_WARN_SUSTAINED_CYCLES
        );
    }

    // ADR-094: `CheckpointOutcomeRecorded` lifecycle event tests.

    #[derive(Default)]
    struct FakeEventStore {
        events: std::sync::Mutex<Vec<khive_storage::Event>>,
    }

    #[async_trait::async_trait]
    impl khive_storage::EventStore for FakeEventStore {
        async fn append_event(
            &self,
            event: khive_storage::Event,
        ) -> khive_storage::StorageResult<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }

        async fn append_events(
            &self,
            events: Vec<khive_storage::Event>,
        ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
            let count = events.len() as u64;
            self.events.lock().unwrap().extend(events);
            Ok(khive_storage::BatchWriteSummary {
                attempted: count,
                affected: count,
                failed: 0,
                first_error: String::new(),
            })
        }

        async fn get_event(
            &self,
            id: uuid::Uuid,
        ) -> khive_storage::StorageResult<Option<khive_storage::Event>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.id == id)
                .cloned())
        }

        async fn query_events(
            &self,
            _filter: khive_storage::EventFilter,
            _page: khive_storage::PageRequest,
        ) -> khive_storage::StorageResult<khive_storage::Page<khive_storage::Event>> {
            unimplemented!("not exercised by the checkpoint lifecycle-event tests")
        }

        async fn count_events(
            &self,
            _filter: khive_storage::EventFilter,
        ) -> khive_storage::StorageResult<u64> {
            Ok(self.events.lock().unwrap().len() as u64)
        }
    }

    /// Pure decision-table coverage for every input combination
    /// `checkpoint_outcome_should_emit` can see: a first elevated tick, a
    /// sustained elevated tick, the single drain row, and the ordinary
    /// healthy tick that must emit nothing.
    #[test]
    fn checkpoint_outcome_should_emit_covers_all_transitions() {
        assert!(
            checkpoint_outcome_should_emit(true, false),
            "first elevated tick must emit"
        );
        assert!(
            checkpoint_outcome_should_emit(true, true),
            "sustained elevated tick must emit"
        );
        assert!(
            checkpoint_outcome_should_emit(false, true),
            "the single drain row (elevated -> healthy) must emit"
        );
        assert!(
            !checkpoint_outcome_should_emit(false, false),
            "an ordinary below-warn tick must not emit"
        );
    }

    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_emits_outcome_events_while_elevated_and_stops_after_drain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcome_emit.db");
        let pool = file_pool(&path);

        // warn_pages: 0 means any observed WAL page count (even 0) is
        // "elevated" for the duration this config is active.
        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: 0,
            ..CheckpointConfig::default()
        };
        let store = Arc::new(FakeEventStore::default());
        let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            Some(store_dyn),
            "local".to_string(),
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        let events = store.events.lock().unwrap();
        assert!(
            !events.is_empty(),
            "an always-elevated config must append at least one CheckpointOutcomeRecorded event"
        );
        assert!(
            events
                .iter()
                .all(|e| e.kind == khive_types::EventKind::CheckpointOutcomeRecorded),
            "every appended event must be CheckpointOutcomeRecorded, got: {events:?}"
        );
        assert!(
            events.iter().all(|e| e.namespace == "local"),
            "events must be stamped with the namespace passed to run_checkpoint_task"
        );
    }

    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_emits_nothing_while_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcome_no_emit.db");
        let pool = file_pool(&path);

        // An unreachable warn_pages threshold for this test's tiny WAL: every
        // tick stays below warn, so no event should ever be appended.
        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: u64::MAX,
            ..CheckpointConfig::default()
        };
        let store = Arc::new(FakeEventStore::default());
        let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            Some(store_dyn),
            "local".to_string(),
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        assert!(
            store.events.lock().unwrap().is_empty(),
            "a config that never crosses warn_pages must never append a lifecycle event"
        );
    }

    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_with_no_event_store_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcome_none_store.db");
        let pool = file_pool(&path);

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: 0,
            ..CheckpointConfig::default()
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            None,
            "local".to_string(),
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(40)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");
    }

    // Round-2 fix (Medium finding 4 + High finding): task-level regressions
    // that actually spawn `run_checkpoint_task` and capture its `tracing`
    // output, so the wiring at the `tx_age_state.observe(...)` call site
    // itself is under test — the pure `TxAgeSweepState` unit tests above
    // stay green even if that call site is deleted; these do not. All three
    // share `#[serial(tx_registry, checkpoint_skip_metrics)]`: `tx_registry`
    // because they read the process-wide registry singleton (see the
    // `log_tx_registry_oldest_debug_reports_oldest_open_entry` doc comment
    // above for why other tests in this same binary can transiently touch
    // it too), `checkpoint_skip_metrics` because they spawn the real task
    // that updates the module's skip-tracking atomics.

    /// (1) A stale labeled entry with a healthy WAL: the spawned task itself
    /// must sweep and escalate it to `Stale`, with WAL-pressure thresholds
    /// set unreachably high so only the age sweep — never the WAL-pressure
    /// ladder — could be responsible for the captured emission.
    #[tokio::test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    async fn checkpoint_task_sweeps_stale_registry_entry_while_wal_is_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx_age_sweep_task_healthy_wal.db");
        let pool = file_pool(&path);

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let _tx_handle = khive_storage::tx_registry::register(Some(
            "checkpoint_task_healthy_wal_sweep_test".to_string(),
        ));

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: u64::MAX,
            high_water_pages: u64::MAX,
            truncate_high_water_pages: u64::MAX,
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(1),
            ..CheckpointConfig::default()
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            None,
            "local".to_string(),
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        drop(_tx_handle);

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|e| {
                e.tx_label.as_deref() == Some("checkpoint_task_healthy_wal_sweep_test")
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.contains("stale-op cap"))
            }),
            "expected the spawned task to sweep and escalate the stale registry entry \
             to Stale on its own, got: {events:?}"
        );
    }

    /// (2) An empty registry must never produce a Plank 1 age emission from
    /// the real spawned task, mirroring the pure
    /// `tx_age_sweep_empty_registry_emits_nothing` unit test above.
    #[tokio::test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    async fn checkpoint_task_emits_no_age_alert_for_an_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx_age_sweep_task_empty_registry.db");
        let pool = file_pool(&path);

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(1),
            ..CheckpointConfig::default()
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            None,
            "local".to_string(),
            shutdown_rx,
        ));

        tokio::time::sleep(Duration::from_millis(40)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().all(|e| e
                .message
                .as_deref()
                .is_none_or(|m| !m.contains("ADR-091 Plank 1"))),
            "an empty registry must never produce a Plank 1 age emission, got: {events:?}"
        );
    }

    /// (3) High-finding regression: a writer-busy tick must NOT silence the
    /// age sweep. Holds the pool's writer mutex (via `pool.try_writer()`,
    /// never released for the task's entire run) across several checkpoint
    /// intervals alongside a stale registered entry, and asserts the age
    /// alert still fires even though `checkpoint_once` observes
    /// `CheckpointTick::Skipped` on every single tick. Before the fix, the
    /// sweep call sat after the `Skipped` early-continue and never ran here.
    #[tokio::test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    async fn checkpoint_task_sweeps_stale_entry_even_when_writer_is_busy_every_tick() {
        reset_checkpoint_metrics_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx_age_sweep_task_writer_busy.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);")
                .unwrap();
        }

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let _tx_handle = khive_storage::tx_registry::register(Some(
            "checkpoint_task_writer_busy_sweep_test".to_string(),
        ));

        // Held for the checkpoint task's entire run, acquired BEFORE spawn
        // (and with no `.await` in between) so the task cannot possibly
        // observe a free writer on any tick.
        let _writer_guard = pool.try_writer().expect("acquire writer for busy hold");

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(1),
            ..CheckpointConfig::default()
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            Arc::clone(&pool),
            cfg,
            None,
            "local".to_string(),
            shutdown_rx,
        ));

        // Several 10ms intervals, every one of them writer-busy.
        tokio::time::sleep(Duration::from_millis(60)).await;

        assert!(
            checkpoint_skipped_ticks() > 0,
            "test setup must actually drive at least one Skipped tick for this \
             regression to mean anything"
        );

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        drop(_writer_guard);
        drop(_tx_handle);

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|e| {
                e.tx_label.as_deref() == Some("checkpoint_task_writer_busy_sweep_test")
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.contains("stale-op cap"))
            }),
            "expected the age sweep to fire even though every tick's writer checkout \
             was skipped, got: {events:?}"
        );
    }
}
