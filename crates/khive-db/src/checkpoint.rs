//! Periodic WAL checkpoint task for the connection pool (ADR-091).
//!
//! Issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick — non-blocking, never
//! waits for readers. A rare, separately-gated escalation may additionally run
//! `PRAGMA wal_checkpoint(TRUNCATE)` once WAL pressure crosses
//! `truncate_high_water_pages` and `truncate_min_interval` has elapsed since
//! the last attempt (Plank 2); both run under the single writer checkout
//! `checkpoint_once` holds for that tick. `checkpoint_once` uses
//! `try_writer_nowait` (zero-wait `try_lock`) so a tick is skipped immediately
//! when the writer mutex is held, rather than blocking — a skipped tick is
//! always preferable to stalling write traffic.
//!
//! `warn_pages` / `high_water_pages` WARNs fire at most once per below→above
//! crossing; a skipped tick leaves crossing state unchanged. An age-based
//! background sweep (Plank 1) additionally checks the oldest span in
//! `khive_storage::tx_registry` against `tx_warn_secs`/`tx_max_age_secs` on
//! every tick (Skipped or Observed) and escalates to `warn!`/`error!` on each
//! below→above crossing — visibility only, nothing here force-closes a stale
//! span.
//!
//! See crates/khive-db/docs/api/checkpoint.md#module-overview-adr-091-planks-012
//! for full ADR-091 Plank 0/1/2 design rationale (why TRUNCATE is excluded
//! from ordinary ticks, the single-writer-checkout invariant, and why Plank 1
//! is a sweep rather than the ADR's originally-described per-statement guard).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pool::{ConnectionPool, WriterGuard};

// ── metrics read-surface (load/perf harness) ─────────────────────────────
// Read-only process-wide gauges (never reset outside #[cfg(test)]). See
// crates/khive-db/docs/api/checkpoint.md#metrics-read-surface-loadperf-harness

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
    /// shared open-transaction registry is surfaced at `tracing::warn!` on
    /// every tick (Skipped or Observed), independent of WAL page pressure.
    /// See `crates/khive-db/docs/api/checkpoint.md` for the Plank 1 rationale.
    ///
    /// Overridable via `KHIVE_TX_WARN_SECS`.
    /// Default: 30 seconds.
    pub tx_warn_secs: Duration,

    /// ADR-091 Plank 1 hard cap: age past which the same sweep escalates the
    /// oldest registry entry to `tracing::error!`. This is visibility only —
    /// nothing here can force-close a stale span; see
    /// `crates/khive-db/docs/design.md` for why.
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

        (cfg.tx_warn_secs, cfg.tx_max_age_secs) =
            tx_age_thresholds_from_env(cfg.tx_warn_secs, cfg.tx_max_age_secs);

        cfg
    }
}

/// Parse `KHIVE_TX_WARN_SECS`/`KHIVE_TX_MAX_AGE_SECS` against the given
/// defaults, applying the same ordering guard both [`CheckpointConfig`] and
/// [`SessionSweepConfig`] need (minor, ADR-091 Amendment 2: this was
/// previously duplicated verbatim in both `from_env` methods).
///
/// The severity ladder assumes `tx_warn_secs < tx_max_age_secs` (Warn fires
/// before Stale as an entry ages). A reversed or equal pair — whether from
/// one misconfigured var or the interaction of both — would invert or
/// collapse that ordering (e.g. WARN_SECS=120, MAX_AGE_SECS=30 emits Stale at
/// 30s and never reaches the Warn crossing until 120s), so both are rejected
/// together rather than silently honored. Resetting both to the caller's
/// defaults (rather than just clamping one) avoids guessing which of the two
/// the operator actually meant to change.
fn tx_age_thresholds_from_env(
    default_warn: Duration,
    default_max: Duration,
) -> (Duration, Duration) {
    let mut warn_secs = default_warn;
    let mut max_age_secs = default_max;

    if let Ok(v) = std::env::var("KHIVE_TX_WARN_SECS") {
        if let Ok(n) = v.parse::<u64>() {
            if n > 0 {
                warn_secs = Duration::from_secs(n);
            }
        }
    }

    if let Ok(v) = std::env::var("KHIVE_TX_MAX_AGE_SECS") {
        if let Ok(n) = v.parse::<u64>() {
            if n > 0 {
                max_age_secs = Duration::from_secs(n);
            }
        }
    }

    if warn_secs >= max_age_secs {
        tracing::warn!(
            configured_tx_warn_secs = warn_secs.as_secs_f64(),
            configured_tx_max_age_secs = max_age_secs.as_secs_f64(),
            fallback_tx_warn_secs = default_warn.as_secs_f64(),
            fallback_tx_max_age_secs = default_max.as_secs_f64(),
            "KHIVE_TX_WARN_SECS must be strictly less than KHIVE_TX_MAX_AGE_SECS; \
             both transaction-age thresholds were rejected and reset to their defaults"
        );
        return (default_warn, default_max);
    }

    (warn_secs, max_age_secs)
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
/// entry across every registered span, regardless of which call site created
/// it. Deliberately a different signal from the WAL-pressure ladder: a span
/// can go stale under low WAL pressure, or vice versa. See
/// `crates/khive-db/docs/api/checkpoint.md` for the full rationale.
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
    /// entry (identity, age, label), or `None` if empty. Returns zero, one,
    /// or two emissions — an entry already stale the first time it's seen
    /// under a given identity crosses both rungs on the same tick.
    ///
    /// A below-threshold (or absent) oldest entry resets both latches. A
    /// change in the oldest entry's [`TxId`](khive_storage::tx_registry::TxId)
    /// also force-resets both latches before re-evaluating age, so a
    /// departed span's latched state cannot suppress the crossing for an
    /// already-stale successor. See `crates/khive-db/docs/api/checkpoint.md`
    /// for why identity tracking is required here, not just the age check.
    pub fn observe(
        &mut self,
        oldest: Option<(khive_storage::tx_registry::TxId, Duration, Option<String>)>,
        tx_warn_secs: Duration,
        tx_max_age_secs: Duration,
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

        let above_warn = age >= tx_warn_secs;
        let above_max_age = age >= tx_max_age_secs;

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

/// ADR-091 Amendment 2 Plank B: per-process walpin sidecar state, carried
/// across ticks by whichever sweep owns it (the daemon's `run_checkpoint_task`
/// or a session's `run_session_sweep_task`). Writes this process's heartbeat
/// on every tick the registry's oldest span exceeds `tx_warn_secs`, and
/// removes it once on the tick the condition clears (and on shutdown) — a
/// process that never crosses the threshold writes nothing.
struct WalpinSidecarState {
    dir: PathBuf,
    pid: u32,
    role: &'static str,
    started_at: i64,
    /// This sweep's own tick cadence, recorded into every beacon and
    /// heartbeat so the enumerating daemon judges freshness against the
    /// PRODUCER's interval — a session on an independently slower configured
    /// cadence must not be misread as stale.
    interval_ms: u64,
    wrote: bool,
    /// Whether this process's registration beacon is believed present on
    /// disk. Cleared when a failed heartbeat write escalates to beacon
    /// removal (fail-closed — see `observe`) or a beacon touch fails; the
    /// next healthy tick then re-registers with a full write instead of a
    /// metadata touch.
    beacon_registered: bool,
}

impl WalpinSidecarState {
    /// `None` when the sidecar is disabled for this backend/env, or the
    /// backend has no on-disk path (in-memory).
    fn new(
        db_path: Option<&Path>,
        is_file_backed: bool,
        role: &'static str,
        interval: Duration,
    ) -> Option<Self> {
        let path = db_path?;
        if !crate::walpin::sidecar_enabled(is_file_backed) {
            return None;
        }
        let pid = std::process::id();
        Some(Self {
            dir: crate::walpin::sidecar_dir_for(path),
            pid,
            role,
            started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
            interval_ms: interval.as_millis().min(u64::MAX as u128) as u64,
            wrote: false,
            beacon_registered: false,
        })
    }

    /// Write this process's registration beacon (ADR-091 Amendment 2
    /// sidecar-health attribution). Called once right after construction,
    /// before the sweep loop starts, and again only when a fail-closed
    /// removal or failed touch cleared `beacon_registered` — steady state
    /// stays metadata-touch-only with no data writes. The blocking fs I/O
    /// runs on `spawn_blocking` (perf, ADR-091 Amendment 2): this is
    /// invoked from an async context and must not run synchronous I/O
    /// inline on the async runtime's worker thread.
    async fn register_beacon(&mut self) {
        let dir = self.dir.clone();
        let beacon = crate::walpin::WalpinBeacon {
            pid: self.pid,
            process_role: self.role.to_string(),
            started_at: self.started_at,
            interval_ms: self.interval_ms,
        };
        let result =
            tokio::task::spawn_blocking(move || crate::walpin::write_beacon(&dir, &beacon)).await;
        match result {
            Ok(Ok(())) => {
                self.beacon_registered = true;
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 2: failed to write walpin registration beacon; \
                     this process's sidecar health will read as unknown, not registered-silent"
                );
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 2: walpin beacon write task panicked"
                );
            }
        }
    }

    /// ADR-091 Amendment 2 beacon refresh rule: a metadata-only mtime touch
    /// of this process's already-registered beacon, performed on every
    /// sweep tick except one where an over-threshold heartbeat write failed
    /// (see `observe`) — `registered-silent` classification requires this
    /// refresh to stay within the freshness window, not just the beacon's
    /// original write. After a fail-closed beacon removal (or a failed
    /// touch), the beacon is re-registered with a full write on the next
    /// healthy tick. Best-effort: a failure here degrades this process to
    /// `unknown` at the next enumeration, not a sweep-task error.
    async fn refresh_beacon(&mut self) {
        if !self.beacon_registered {
            self.register_beacon().await;
            return;
        }
        let dir = self.dir.clone();
        let pid = self.pid;
        let result =
            tokio::task::spawn_blocking(move || crate::walpin::touch_beacon(&dir, pid)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.beacon_registered = false;
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 2: failed to refresh walpin registration beacon; \
                     this process's sidecar health will read as unknown, not registered-silent"
                );
            }
            Err(join_err) => {
                self.beacon_registered = false;
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 2: walpin beacon refresh task panicked"
                );
            }
        }
    }

    /// Fail-closed escalation for a failed heartbeat write: remove this
    /// process's beacon so enumeration cannot classify it
    /// `registered-silent` off the still-fresh prior refresh — skipping one
    /// touch alone leaves the previous mtime inside the freshness window
    /// for up to three producer ticks, an exoneration window. With the
    /// beacon gone the process either reports (once writes recover, the
    /// next tick re-registers and writes the heartbeat) or is caught by the
    /// OS-level holder census as an unattributed holder. If the removal
    /// itself fails, the beacon ages out over the freshness window — the
    /// narrowed fallback, not the contract.
    async fn drop_beacon_fail_closed(&mut self) {
        let dir = self.dir.clone();
        let pid = self.pid;
        self.beacon_registered = false;
        let result =
            tokio::task::spawn_blocking(move || crate::walpin::remove_beacon(&dir, pid)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 2: failed to remove walpin beacon after a failed \
                     heartbeat write; beacon will age out of the freshness window instead"
                );
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 2: walpin beacon removal task panicked"
                );
            }
        }
    }

    /// Blocking heartbeat write/removal runs on `spawn_blocking` (perf,
    /// ADR-091 Amendment 2) — this async sweep task must not block its
    /// executor thread on synchronous filesystem I/O.
    async fn observe(
        &mut self,
        oldest: Option<khive_storage::tx_registry::OldestSpan>,
        tx_warn_secs: Duration,
    ) {
        match oldest {
            Some(span) if span.age >= tx_warn_secs => {
                // ADR-091 Amendment 3 Plank F2: the caller's `TxOriginFilter`
                // guarantees a `Main` view's winner is either `Database` (this
                // backend's own identity) or `Unscoped` (the fallback), and a
                // `Secondary` view's winner is always `Database` — `Memory`
                // can never win a filtered query, so it degrades to
                // fallback-confidence rather than a reachability panic.
                let attribution_basis = match span.origin {
                    khive_storage::tx_registry::TxOrigin::Database(_) => "origin",
                    khive_storage::tx_registry::TxOrigin::Unscoped
                    | khive_storage::tx_registry::TxOrigin::Memory => "fallback",
                };
                let heartbeat = crate::walpin::WalpinHeartbeat {
                    pid: self.pid,
                    process_role: self.role.to_string(),
                    started_at: self.started_at,
                    oldest_tx_age_secs: span.age.as_secs_f64(),
                    oldest_tx_label: span.label,
                    updated_at: now_epoch_secs(),
                    interval_ms: self.interval_ms,
                    attribution_basis: Some(attribution_basis.to_string()),
                };
                let dir = self.dir.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::walpin::write_heartbeat(&dir, &heartbeat)
                })
                .await;
                // The beacon refresh is gated on the heartbeat write
                // landing: a fresh beacon with no heartbeat file classifies
                // as `registered-silent` at enumeration, so a failed write
                // would exonerate a process that currently holds an
                // over-threshold transaction. Skipping the refresh alone is
                // not enough — the previous touch stays inside the freshness
                // window for up to three producer ticks — so the failure
                // path removes the beacon outright (`drop_beacon_fail_closed`);
                // the next successful tick re-registers it.
                match result {
                    Ok(Ok(())) => {
                        self.wrote = true;
                        self.refresh_beacon().await;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            "ADR-091 Amendment 2 Plank B: failed to write walpin heartbeat; \
                             removing beacon so this process cannot read as \
                             registered-silent while over threshold"
                        );
                        self.drop_beacon_fail_closed().await;
                    }
                    Err(join_err) => {
                        tracing::warn!(
                            error = %join_err,
                            "ADR-091 Amendment 2 Plank B: walpin heartbeat write task panicked"
                        );
                        self.drop_beacon_fail_closed().await;
                    }
                }
            }
            _ => {
                self.refresh_beacon().await;
                if self.wrote {
                    let dir = self.dir.clone();
                    let pid = self.pid;
                    let result = tokio::task::spawn_blocking(move || {
                        crate::walpin::remove_heartbeat(&dir, pid)
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(
                            error = %e,
                            "ADR-091 Amendment 2 Plank B: failed to remove walpin heartbeat"
                        ),
                        Err(join_err) => tracing::warn!(
                            error = %join_err,
                            "ADR-091 Amendment 2 Plank B: walpin heartbeat removal task panicked"
                        ),
                    }
                    self.wrote = false;
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        if self.wrote {
            let dir = self.dir.clone();
            let pid = self.pid;
            let _ = tokio::task::spawn_blocking(move || crate::walpin::remove_heartbeat(&dir, pid))
                .await;
            self.wrote = false;
        }
    }
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// ADR-091 Amendment 2 Plank A: config for the observe-only per-session
/// sweep. Sessions never checkpoint — that stays daemon-owned so N session
/// processes never compete for the writer mutex — this only watches
/// `tx_registry` (and, Plank B, refreshes this process's walpin heartbeat).
#[derive(Clone, Debug)]
pub struct SessionSweepConfig {
    /// How often a session polls the registry. Coarser than the daemon's
    /// tick: sessions do not need the daemon's 500ms checkpoint cadence.
    ///
    /// Overridable via `KHIVE_SESSION_SWEEP_INTERVAL_MS`. Default: 5000 ms.
    pub interval: Duration,
    /// Same semantics and default as [`CheckpointConfig::tx_warn_secs`].
    pub tx_warn_secs: Duration,
    /// Same semantics and default as [`CheckpointConfig::tx_max_age_secs`].
    pub tx_max_age_secs: Duration,
}

impl Default for SessionSweepConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            tx_warn_secs: Duration::from_secs(30),
            tx_max_age_secs: Duration::from_secs(120),
        }
    }
}

impl SessionSweepConfig {
    /// Build from the environment. Reuses `KHIVE_TX_WARN_SECS` /
    /// `KHIVE_TX_MAX_AGE_SECS` (the same knobs the daemon's checkpoint task
    /// reads) so a session and the daemon agree on the same thresholds.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(ms) = std::env::var("KHIVE_SESSION_SWEEP_INTERVAL_MS") {
            if let Ok(v) = ms.parse::<u64>() {
                if v > 0 {
                    cfg.interval = Duration::from_millis(v);
                }
            }
        }
        // Shares `tx_age_thresholds_from_env` with `CheckpointConfig::from_env`
        // (minor, ADR-091 Amendment 2) so a session and the daemon
        // parse and validate `KHIVE_TX_WARN_SECS`/`KHIVE_TX_MAX_AGE_SECS`
        // identically from one source, not two hand-copied blocks.
        (cfg.tx_warn_secs, cfg.tx_max_age_secs) =
            tx_age_thresholds_from_env(cfg.tx_warn_secs, cfg.tx_max_age_secs);

        cfg
    }
}

/// One file-backed backend the session sweep observes (ADR-091 Amendment 3
/// fan-out). `is_main` selects which [`khive_storage::tx_registry::TxOriginFilter`]
/// variant scopes this backend's view of the registry: the main backend's
/// `Main` filter additionally observes `Unscoped` spans (the
/// never-silently-drop fallback for call sites not yet threaded to an
/// origin); a secondary backend's `Secondary` filter is scoped to exactly
/// its own identity. A pool whose origin is `Memory` contributes no entry —
/// in-memory backends have no sidecar and nothing to attribute
/// cross-process.
pub struct SweepBackend {
    pub pool: Arc<ConnectionPool>,
    pub is_main: bool,
}

/// Per-backend state the session sweep carries across ticks: this backend's
/// registry view, its own edge-triggered age-sweep state machine (so a
/// sustained stale span on one backend logs independently of the others),
/// and its own walpin sidecar (`None` if the sidecar is disabled or this
/// backend's origin is `Memory`).
struct BackendSweep {
    filter: khive_storage::tx_registry::TxOriginFilter,
    tx_age_state: TxAgeSweepState,
    sidecar: Option<WalpinSidecarState>,
}

/// ADR-091 Amendment 2 Plank A (Amendment 3: per-backend fan-out): run the
/// observe-only per-session sweep.
///
/// Every non-daemon `kkernel mcp` process runs this instead of the daemon's
/// `run_checkpoint_task`: same `tx_registry` age check and Plank B heartbeat
/// refresh, but no PASSIVE/TRUNCATE checkpointing — checkpointing stays
/// daemon-owned. Stays ONE task for the whole process, but fans out
/// internally: each file-backed backend in `backends` gets its own
/// registry view, age-sweep state, and sidecar directory, so a long span on
/// a secondary backend is attributed (and heartbeats) only in that
/// backend's own sidecar — never the main backend's. Loops until
/// `shutdown_rx` observes a change (or its sender is dropped), removing
/// every written heartbeat on the way out.
pub async fn run_session_sweep_task(
    backends: Vec<SweepBackend>,
    config: SessionSweepConfig,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
) {
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sweeps: Vec<BackendSweep> = Vec::with_capacity(backends.len());
    for backend in backends {
        let identity = match backend.pool.origin() {
            khive_storage::tx_registry::TxOrigin::Database(id) => id,
            // No on-disk file, so no sidecar and no cross-process
            // attribution surface — nothing for this sweep to fan out to.
            khive_storage::tx_registry::TxOrigin::Memory
            | khive_storage::tx_registry::TxOrigin::Unscoped => continue,
        };
        let filter = if backend.is_main {
            khive_storage::tx_registry::TxOriginFilter::Main(identity)
        } else {
            khive_storage::tx_registry::TxOriginFilter::Secondary(identity)
        };
        let sidecar = WalpinSidecarState::new(
            backend.pool.canonical_path(),
            true,
            "session",
            config.interval,
        );
        sweeps.push(BackendSweep {
            filter,
            tx_age_state: TxAgeSweepState::default(),
            sidecar,
        });
    }
    for sweep in sweeps.iter_mut() {
        if let Some(sidecar) = sweep.sidecar.as_mut() {
            sidecar.register_beacon().await;
        }
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.changed() => break,
        }

        for sweep in sweeps.iter_mut() {
            let oldest = khive_storage::tx_registry::oldest_for(&sweep.filter);
            for emission in sweep.tx_age_state.observe(
                oldest.as_ref().map(|s| (s.id, s.age, s.label.clone())),
                config.tx_warn_secs,
                config.tx_max_age_secs,
            ) {
                log_tx_age_emission(&emission);
            }
            if let Some(sidecar) = sweep.sidecar.as_mut() {
                sidecar.observe(oldest, config.tx_warn_secs).await;
            }
        }
    }

    for sweep in sweeps.iter_mut() {
        if let Some(sidecar) = sweep.sidecar.as_mut() {
            sidecar.shutdown().await;
        }
    }
}

/// Run the WAL checkpoint background task.
///
/// Long-running async task — spawn with `tokio::spawn`. Loops until
/// `shutdown_rx` observes a change (or its sender is dropped). Callers MUST
/// hold the paired `tokio::sync::watch::Sender` for the daemon's run scope
/// and send on it to shut down — do NOT rely on `pool`'s `Arc` refcount
/// reaching zero; a sibling owner (e.g. `event_store`) holding its own clone
/// makes that check unreachable (issue #774).
///
/// Issues `PRAGMA wal_checkpoint(PASSIVE)` every tick via `try_writer_nowait`
/// (zero-wait try-lock): a busy writer skips the tick rather than stalling
/// write traffic. A WARNING fires once per below→above threshold crossing,
/// not every tick.
///
/// `event_store` (ADR-094): when `Some`, appends a best-effort
/// `CheckpointOutcomeRecorded` event on every at/above-`warn_pages` tick,
/// plus one drain row when pressure falls back below `warn_pages`. `None` is
/// a no-op. See `crates/khive-db/docs/api/checkpoint.md` for the full
/// shutdown-mechanism and event-emission design history.
///
/// `is_main` (ADR-091 Amendment 3): whether `pool` is the deployment's main
/// backend. A daemon owning several file-backed backends spawns one task per
/// backend, each with its own pool and shutdown-channel clone (the sender
/// broadcasts to every receiver clone alike); exactly one of those calls
/// passes `true`. See the `tx_filter` construction below for what this
/// selects.
pub async fn run_checkpoint_task(
    pool: Arc<ConnectionPool>,
    config: CheckpointConfig,
    event_store: Option<Arc<dyn khive_storage::EventStore>>,
    namespace: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
    is_main: bool,
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
    // ADR-091 Amendment 3: this task's own backend-scoped view of the
    // registry. `is_main` selects which `TxOriginFilter` variant applies —
    // the caller passes `true` for exactly the one checkpoint task covering
    // the deployment's main backend, so only that task also observes legacy
    // `Unscoped` spans from any call site not yet threaded to an origin, the
    // designed never-silently-drop fallback. A secondary backend's task
    // never falls back to `Unscoped`: those spans belong to the main view or
    // to no view, never to a database they were never registered against.
    // `None` only when this pool's own origin isn't `Database` (an in-memory
    // checkpoint pool) — degrades to "no open span observed" for the tick
    // rather than panicking a long-running daemon loop on an
    // assumed-impossible state.
    let tx_filter = match pool.origin() {
        khive_storage::tx_registry::TxOrigin::Database(id) => Some(if is_main {
            khive_storage::tx_registry::TxOriginFilter::Main(id)
        } else {
            khive_storage::tx_registry::TxOriginFilter::Secondary(id)
        }),
        khive_storage::tx_registry::TxOrigin::Memory
        | khive_storage::tx_registry::TxOrigin::Unscoped => None,
    };
    // ADR-091 Amendment 2 Plank B: the checkpoint pool is only ever wired for
    // file-backed backends (`checkpoint_pool_for`), so `is_file_backed: true`
    // is always correct here. `canonical_path()` (not `pool.config().path`)
    // so the sidecar directory is keyed off the same minted identity every
    // alias of this backend's configured path converges to.
    #[cfg(unix)]
    let mut walpin_state =
        WalpinSidecarState::new(pool.canonical_path(), true, "daemon", config.interval);
    #[cfg(unix)]
    if let Some(sidecar) = walpin_state.as_mut() {
        sidecar.register_beacon().await;
    }

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
        let oldest_tx = tx_filter
            .as_ref()
            .and_then(khive_storage::tx_registry::oldest_for);
        for emission in tx_age_state.observe(
            oldest_tx.as_ref().map(|s| (s.id, s.age, s.label.clone())),
            config.tx_warn_secs,
            config.tx_max_age_secs,
        ) {
            log_tx_age_emission(&emission);
        }
        // ADR-091 Amendment 2 Plank B: refresh (or clear) this daemon
        // process's own walpin heartbeat on the same cadence, so its own
        // pin — if any — is attributable the same way a session's is.
        #[cfg(unix)]
        if let Some(sidecar) = walpin_state.as_mut() {
            sidecar
                .observe(oldest_tx.clone(), config.tx_warn_secs)
                .await;
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

        // Per-tick debug for the oldest open entry always fires (cheap —
        // reuses this tick's already-computed `oldest_tx`); the two
        // `warn!`-level registry logs below are gated on the SAME crossing
        // state as the WAL-threshold WARNs above, so sustained pressure
        // logs once per crossing, not once per tick.
        log_tx_registry_oldest_debug(wal_pages, oldest_tx.as_ref());

        // ADR-091 severity ladder: INFO on the first below→above crossing,
        // WARN once `warn_sustained_cycles` consecutive ticks stay elevated.
        // The oldest-entry registry WARN rides the same INFO edge the old
        // binary crossing_warn used to gate on.
        for emission in severity_state.observe_wal_pages(wal_pages, &config) {
            match emission.rung {
                CheckpointSeverityRung::Info => {
                    log_tx_registry_oldest_warn(wal_pages, oldest_tx.as_ref());
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

    #[cfg(unix)]
    if let Some(sidecar) = walpin_state.as_mut() {
        sidecar.shutdown().await;
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

/// ADR-091 Plank 0 (Amendment 3: takes the tick's already-computed,
/// backend-scoped oldest span instead of re-querying the process-wide
/// aggregate): log the oldest open transaction registry entry alongside the
/// WAL frame count at `debug!`, on EVERY tick regardless of threshold
/// state. This is the low-volume per-tick trace; the WARN-level escalations
/// live in [`log_tx_registry_oldest_warn`] and
/// debug-level, unconditional per-tick trace. See
/// crates/khive-db/docs/api/checkpoint.md#private-tx-registry-logging-helpers-plank-0
fn log_tx_registry_oldest_debug(
    wal_pages: u64,
    oldest: Option<&khive_storage::tx_registry::OldestSpan>,
) {
    if let Some(span) = oldest {
        tracing::debug!(
            wal_pages,
            oldest_tx_age_secs = span.age.as_secs_f64(),
            oldest_tx_label = span.label.as_deref().unwrap_or("<unlabeled>"),
            "WAL checkpoint tick: oldest open transaction registry entry"
        );
    }
}

/// Escalates the oldest open registry entry to `warn!`. NOT internally
/// rate-limited — caller MUST gate on a below→above `warn_pages` crossing
/// (`crossing_warn`) or every tick reproduces the log-spam bug this fixes.
fn log_tx_registry_oldest_warn(
    wal_pages: u64,
    oldest: Option<&khive_storage::tx_registry::OldestSpan>,
) {
    if let Some(span) = oldest {
        tracing::warn!(
            wal_pages,
            oldest_tx_age_secs = span.age.as_secs_f64(),
            oldest_tx_label = span.label.as_deref().unwrap_or("<unlabeled>"),
            "WAL checkpoint tick: oldest open transaction registry entry"
        );
    }
}

/// Enumerates every open registry entry at `warn!`. NOT internally
/// rate-limited — caller MUST gate on a below→above `high_water_pages`
/// crossing (`crossing_warn`) or every tick repeats the full enumeration.
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

/// Evaluate and, if due, attempt a TRUNCATE escalation under the writer
/// guard the caller already holds (never its own checkout). `last_attempt`
/// is stamped ONLY on an actual attempt, never on a skip. See
/// crates/khive-db/docs/api/checkpoint.md#maybe_truncate--truncate-attempt-gating-plank-2
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
                #[cfg(unix)]
                log_walpin_sidecar_report(pool);
                log_wal_pin_depth(conn);
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

/// ADR-091 Amendment 2 Plank B: on a TRUNCATE no-progress event, enumerate
/// the walpin sidecar directory and log every entry's sidecar-health
/// classification (reporting / registered-silent / unknown), attributing the
/// pin to a specific cross-process PID rather than only this process's own
/// registry. A no-op if the sidecar is disabled or this backend has no
/// on-disk path.
///
/// Sidecar-health attribution (ADR-091 Amendment 2):
/// the sharper "unregistered/native mechanism" conclusion is licensed only
/// when every discovered PID is `reporting` or `registered-silent`
/// (`WalpinReport::fully_attributed`); any `unknown` PID — including the
/// directory itself failing the trust-boundary check — makes attribution
/// inconclusive, and the WARN below names exactly which PIDs are unresolved
/// instead of silently exonerating them.
#[cfg(unix)]
fn log_walpin_sidecar_report(pool: &ConnectionPool) {
    let Some(path) = pool.canonical_path() else {
        return;
    };
    if !crate::walpin::sidecar_enabled(true) {
        return;
    }
    let dir = crate::walpin::sidecar_dir_for(path);
    // Each record carries its producer's own sweep cadence (`interval_ms`),
    // which is what freshness is judged against; the interval passed here is
    // only the fallback for records written before that field existed.
    let sweep_interval = SessionSweepConfig::from_env().interval;
    let report = match crate::walpin::enumerate_live(&dir, sweep_interval) {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ADR-091 Amendment 2 Plank B: sidecar directory failed the trust-boundary \
                 check; cross-process WAL-pin attribution is unestablished for this tick"
            );
            return;
        }
    };
    for hb in report.reporting() {
        tracing::warn!(
            walpin_pid = hb.pid,
            walpin_role = %hb.process_role,
            walpin_oldest_tx_age_secs = hb.oldest_tx_age_secs,
            walpin_oldest_tx_label = hb.oldest_tx_label.as_deref().unwrap_or("<unlabeled>"),
            walpin_health = "reporting",
            "ADR-091 Amendment 2 Plank B: live cross-process WAL-pin attribution report"
        );
    }
    for pid in report.registered_silent_pids() {
        tracing::debug!(
            walpin_pid = pid,
            walpin_health = "registered_silent",
            "ADR-091 Amendment 2 Plank B: process affirmatively reports no over-threshold span"
        );
    }
    let mut unknown_pids: Vec<u32> = report.unknown_pids().collect();

    // ADR-091 Amendment 2 (OS-derived census): the sidecar
    // directory alone can only speak for PIDs that wrote SOMETHING there —
    // a database holder that never registered a beacon at all (pre-feature
    // binary, sidecar disabled, wedged before its first write) would
    // otherwise be invisible. Widen the universe to every PID the OS
    // reports as currently holding the database file open; any such PID
    // absent from `report` entirely is `unknown` for the same reason a
    // stale/unowned sidecar entry is.
    match crate::walpin::census_holders(path) {
        Ok(census) => {
            let sidecar_known: std::collections::HashSet<u32> = report
                .reporting()
                .map(|hb| hb.pid)
                .chain(report.registered_silent_pids())
                .chain(unknown_pids.iter().copied())
                .collect();
            let mut census_only: Vec<u32> =
                census.holders.difference(&sidecar_known).copied().collect();
            if !census_only.is_empty() {
                census_only.sort_unstable();
                tracing::warn!(
                    ?census_only,
                    "ADR-091 Amendment 2: these PIDs hold the database file open \
                     at the OS level but have no sidecar data at all (pre-feature binary, \
                     sidecar disabled, or wedged before its first write)"
                );
                unknown_pids.extend(census_only);
            }
            if !census.is_complete() {
                let mut uninspectable = census.uninspectable_pids.clone();
                uninspectable.sort_unstable();
                tracing::warn!(
                    ?uninspectable,
                    truncated = census.truncated,
                    "ADR-091 Amendment 2: the OS-derived holder census is \
                     INCOMPLETE — either specific PIDs' open file descriptors could not be \
                     inspected (permission denied, or a listing race), or the enumeration walk \
                     itself has positive evidence it did not see the full live-process universe \
                     (namespace/visibility check, directory-iterator error, self-canary, or a \
                     libproc buffer that stayed at capacity after bounded retries) — cannot \
                     rule out an unregistered holder"
                );
                if uninspectable.is_empty() {
                    // `truncated` fired with no specific PID list (a
                    // namespace/visibility or buffer-truncation signal, not
                    // a per-PID inspection failure) — still makes
                    // attribution inconclusive. Mirror the census-failure
                    // arm below with the same non-PID sentinel rather than
                    // silently trusting a walk we know was incomplete.
                    unknown_pids.push(0);
                } else {
                    unknown_pids.extend(uninspectable);
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ADR-091 Amendment 2: OS-derived holder census failed; \
                 attribution cannot rule out an unregistered database holder this tick"
            );
            // A failed census is itself a health failure for the sharper
            // conclusion below — treat it as if at least one PID were
            // unresolved, without fabricating a specific PID number.
            unknown_pids.push(0);
        }
    }

    if !unknown_pids.is_empty() {
        tracing::warn!(
            ?unknown_pids,
            "ADR-091 Amendment 2 Plank B: sidecar health unestablished for these PIDs; \
             attribution is inconclusive and the native/unregistered-mechanism conclusion \
             is NOT licensed this tick"
        );
    } else if report.reporting().next().is_none() {
        tracing::info!(
            "ADR-091 Amendment 2 Plank B: every live PID is reporting or registered-silent \
             with none pinning; the WAL pin is not attributable to any in-process registry \
             span this sidecar covers"
        );
    }
}

/// ADR-091 Amendment 2 Plank C: on a TRUNCATE no-progress event, run a fresh
/// `PRAGMA wal_checkpoint(PASSIVE)` (never blocks readers or writers) and
/// report pin depth as `log` minus `checkpointed` from its 3-column return
/// row — the number of frames pinned behind the backfill boundary. Zero
/// dependence on SQLite's shm WAL-index layout.
fn log_wal_pin_depth(conn: &rusqlite::Connection) {
    match query_wal_pin_depth(conn) {
        Ok((log, checkpointed)) => {
            tracing::warn!(
                wal_log_frames = log,
                wal_checkpointed_frames = checkpointed,
                wal_pin_depth = (log - checkpointed).max(0),
                "ADR-091 Amendment 2 Plank C: WAL pin depth after TRUNCATE no-progress"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ADR-091 Amendment 2 Plank C: failed to query WAL pin depth"
            );
        }
    }
}

/// ADR-091 Amendment 2 Plank C: issue `PRAGMA wal_checkpoint(PASSIVE)` and
/// return its `(log, checkpointed)` columns (index 1 and 2 of the 3-column
/// return row). PASSIVE never blocks readers or writers. Pin depth is
/// `log - checkpointed`; extracted as its own pure query so the arithmetic is
/// unit-testable against a real SQLite connection without depending on
/// `tracing` capture.
fn query_wal_pin_depth(conn: &rusqlite::Connection) -> rusqlite::Result<(i64, i64)> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
    })
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

    /// `log_tx_registry_oldest_debug` names the oldest open registry entry.
    /// See crates/khive-db/docs/api/checkpoint.md#log_tx_registry_oldest_debug_reports_oldest_open_entry
    #[test]
    #[serial(tx_registry)]
    fn log_tx_registry_oldest_debug_reports_oldest_open_entry() {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        let _handle =
            khive_storage::tx_registry::register(Some("checkpoint_tick_test".to_string()));

        let oldest = khive_storage::tx_registry::oldest().map(|(id, age, label)| {
            khive_storage::tx_registry::OldestSpan {
                id,
                age,
                label,
                origin: khive_storage::tx_registry::TxOrigin::Unscoped,
            }
        });
        let expected_label = oldest
            .as_ref()
            .and_then(|s| s.label.clone())
            .unwrap_or_else(|| "<unlabeled>".to_string());

        tracing::subscriber::with_default(subscriber, || {
            log_tx_registry_oldest_debug(100, oldest.as_ref());
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

    /// ADR-091 Plank 0: the oldest-entry WARN and the
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
        let oldest = khive_storage::tx_registry::oldest().map(|(id, age, label)| {
            khive_storage::tx_registry::OldestSpan {
                id,
                age,
                label,
                origin: khive_storage::tx_registry::TxOrigin::Unscoped,
            }
        });

        let mut was_above_warn = false;
        let mut was_above_high_water = false;

        tracing::subscriber::with_default(subscriber, || {
            // Tick 1: below→above crossing for both bands — both WARNs fire.
            if crossing_warn(true, &mut was_above_warn) {
                log_tx_registry_oldest_warn(6000, oldest.as_ref());
            }
            if crossing_warn(true, &mut was_above_high_water) {
                log_tx_registry_snapshot_warn(6000);
            }

            // Tick 2: still above both thresholds — neither must repeat.
            if crossing_warn(true, &mut was_above_warn) {
                log_tx_registry_oldest_warn(6000, oldest.as_ref());
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
            true,
        ));

        shutdown_tx.send(()).expect("send shutdown signal");

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");
    }

    /// Regression #774: exits via watch-signal even with a live event_store
    /// pool clone (rules out a strong-count-based exit condition). See
    /// crates/khive-db/docs/api/checkpoint.md#checkpoint_task_exits_via_shutdown_signal_with_live_event_store_pool_clone
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
            true,
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

    /// Regression: a high-water tick must NOT block behind an active read
    /// transaction (isomorphism guarantee — fails if `checkpoint_once`
    /// regresses to TRUNCATE). See
    /// crates/khive-db/docs/api/checkpoint.md#checkpoint_high_water_does_not_block_behind_reader
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

    /// Fix: a reversed threshold pair must not be honored independently. See
    /// crates/khive-db/docs/api/checkpoint.md#checkpoint_config_rejects_reversed_tx_thresholds
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

    /// Degenerate equal-thresholds case; see
    /// crates/khive-db/docs/api/checkpoint.md#checkpoint_config_rejects_equal_tx_thresholds
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

    /// Regression: a Skipped tick must NOT reset `was_above_high_water`. See
    /// crates/khive-db/docs/api/checkpoint.md#skipped_tick_does_not_reset_high_water_crossing_state
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

    /// Regression: warn_pages WARN fires once on crossing, not every tick.
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

    /// Regression guard for #845 (a recurrence of the #828 shared-statics
    /// race): every test in this module that calls `checkpoint_once` or
    /// `run_checkpoint_task` — both funnel through `query_wal_pages`, which
    /// writes the process-wide `LAST_WAL_PAGES` / `CHECKPOINT_*` atomics —
    /// must be tagged with a `#[serial(...)]` group that includes
    /// `checkpoint_skip_metrics`. Before #828, six such call sites carried no
    /// serial tag at all: cargo's default test thread pool ran them
    /// concurrently with `busy_writer_skips_both_passive_and_truncate`, and an
    /// untagged tick's `query_wal_pages` call clobbered the gauges between
    /// this test's warmup tick and its skip assertion (`left: Some(0), right:
    /// Some(3)` on CI — the two ticks never actually raced against each
    /// other, a third test's tick did). This scans the module's own source so
    /// a future test that calls either function without the tag fails this
    /// assertion instead of flaking on a loaded CI runner.
    #[test]
    fn all_checkpoint_metrics_callers_are_serial_tagged() {
        const SELF_SRC: &str = include_str!("checkpoint.rs");
        let lines: Vec<&str> = SELF_SRC.lines().collect();

        let attr_starts: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim();
                t == "#[test]" || t.starts_with("#[tokio::test")
            })
            .map(|(i, _)| i)
            .collect();

        let mut offenders = Vec::new();

        for (idx, &start) in attr_starts.iter().enumerate() {
            let end = attr_starts.get(idx + 1).copied().unwrap_or(lines.len());
            let span = &lines[start..end];

            let touches_shared_metrics = span
                .iter()
                .any(|l| l.contains("checkpoint_once(") || l.contains("run_checkpoint_task("));
            if !touches_shared_metrics {
                continue;
            }

            let has_group_tag = span
                .iter()
                .any(|l| l.contains("#[serial") && l.contains("checkpoint_skip_metrics"));

            if !has_group_tag {
                let name = span
                    .iter()
                    .find_map(|l| {
                        let t = l.trim_start();
                        let t = t.strip_prefix("pub(crate) ").unwrap_or(t);
                        let t = t.strip_prefix("pub ").unwrap_or(t);
                        let t = t.strip_prefix("async ").unwrap_or(t);
                        t.strip_prefix("fn ")
                            .map(|rest| rest.split(['(', '<']).next().unwrap_or("").trim())
                    })
                    .unwrap_or("<unknown test>");
                offenders.push(name.to_string());
            }
        }

        assert!(
            offenders.is_empty(),
            "these tests call checkpoint_once/run_checkpoint_task (which write the \
             process-wide LAST_WAL_PAGES/CHECKPOINT_* atomics via query_wal_pages) but \
             are not tagged #[serial(checkpoint_skip_metrics)] (or a group including it); \
             an untagged caller running concurrently on cargo's default test thread pool \
             can clobber those atomics mid-assertion in another test (the #828/#845 race): \
             {offenders:?}"
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

        let emissions = state.observe(None, config.tx_warn_secs, config.tx_max_age_secs);
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
        );

        let tick = state.observe(
            Some((
                tx_id(1),
                Duration::from_secs(130),
                Some("stuck_writer_task_tx".to_string()),
            )),
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
        );

        // The stale span closed; nothing is open now.
        let cleared = state.observe(None, config.tx_warn_secs, config.tx_max_age_secs);
        assert!(cleared.is_empty(), "a clearing tick must emit nothing");

        // A fresh entry (unrelated span) is now oldest — still below threshold.
        let fresh = state.observe(
            Some((
                tx_id(2),
                Duration::from_secs(2),
                Some("second_span".to_string()),
            )),
            config.tx_warn_secs,
            config.tx_max_age_secs,
        );
        assert!(fresh.is_empty(), "a fresh oldest entry must emit nothing");

        // That second span goes stale in turn — must WARN again (re-armed).
        let rewarn = state.observe(
            Some((
                tx_id(2),
                Duration::from_secs(35),
                Some("second_span".to_string()),
            )),
            config.tx_warn_secs,
            config.tx_max_age_secs,
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

    /// Fix: an already-stale entry replacing a stale one on the next tick,
    /// with no intervening clear, must still emit both rungs. See
    /// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_stale_replacement_without_intervening_clear_still_names_new_entry
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
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

    /// Closes the loop from env var to actual emitted rung. See
    /// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_uses_configured_thresholds_not_hardcoded_defaults
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
            config.tx_warn_secs,
            config.tx_max_age_secs,
        );
        assert_eq!(
            tick.len(),
            2,
            "a millisecond-scale cap must cross both rungs immediately, got: {tick:?}"
        );
    }

    /// Integration-level regression for the incident this ADR fixes. See
    /// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_names_long_lived_reader_pinning_wal_past_high_water
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
        // pinning reader at the Stale rung. The handle's age must exceed the
        // 1ms `tx_max_age_secs` cap deterministically: the inserts plus one
        // PASSIVE checkpoint above can complete in under a millisecond on a
        // warm page cache, so sleep past the cap instead of assuming
        // the elapsed work already crossed it.
        std::thread::sleep(Duration::from_millis(5));
        // `tx_registry` is a process-wide singleton shared by every test in
        // this binary (cargo runs `#[test]`s in parallel threads of the same
        // process): `#[serial(tx_registry)]` only excludes other tests that
        // carry the same key, not every production write path elsewhere in
        // the crate (e.g. `graph_upsert_edges`) that also calls `register()`
        // as ordinary telemetry. If one of those happens to still be open and
        // was registered before this test's own handle, raw `oldest()` would
        // return THAT entry instead of the fixture's reader — see #926. Look
        // up this test's own entry by its known label instead of trusting
        // global `oldest()`, so the assertion is immune to that noise.
        let our_entry = khive_storage::tx_registry::snapshot()
            .into_iter()
            .find(|(_, label)| label.as_deref() == Some("tx_age_sweep_reader_pin_test"))
            .expect("this test's own tx_registry entry must still be open");
        let mut tx_age_state = TxAgeSweepState::default();
        let emissions = tx_age_state.observe(
            Some((tx_id(1), our_entry.0, our_entry.1)),
            config.tx_warn_secs,
            config.tx_max_age_secs,
        );
        assert!(
            emissions.iter().any(|e| e.rung == TxAgeRung::Stale
                && e.label.as_deref() == Some("tx_age_sweep_reader_pin_test")),
            "expected a Stale emission naming the pinning reader, got: {emissions:?}"
        );

        reader.execute_batch("COMMIT;").ok();
        drop(reader);
        drop(_tx_handle);
    }

    /// Regression #926: reproduces the exact tx_registry race directly. See
    /// crates/khive-db/docs/api/checkpoint.md#tx_age_sweep_own_entry_survives_concurrent_older_registration
    #[test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    fn tx_age_sweep_own_entry_survives_concurrent_older_registration() {
        let _decoy = khive_storage::tx_registry::register(Some("decoy_unrelated_span".to_string()));
        std::thread::sleep(Duration::from_millis(2));
        let _own = khive_storage::tx_registry::register(Some("this_test_own_span".to_string()));
        std::thread::sleep(Duration::from_millis(5));

        // Confirm the race condition is actually reproduced: an entry older
        // than this test's own span must currently lead the process-wide
        // registry. Another concurrently running test may have registered an
        // entry before the decoy, so do not assume the decoy is globally
        // oldest; the required invariant is only that our span is not.
        let global_oldest = khive_storage::tx_registry::oldest().expect("registry not empty");
        assert_ne!(
            global_oldest.2.as_deref(),
            Some("this_test_own_span"),
            "test setup must reproduce the race: an older, unrelated entry must be \
             the current global oldest, got: {global_oldest:?}"
        );

        let our_entry = khive_storage::tx_registry::snapshot()
            .into_iter()
            .find(|(_, label)| label.as_deref() == Some("this_test_own_span"))
            .expect("this test's own tx_registry entry must still be open");

        let config = CheckpointConfig {
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(1),
            ..CheckpointConfig::default()
        };
        let mut state = TxAgeSweepState::default();
        let emissions = state.observe(
            Some((tx_id(2), our_entry.0, our_entry.1)),
            config.tx_warn_secs,
            config.tx_max_age_secs,
        );
        assert!(
            emissions
                .iter()
                .any(|e| e.rung == TxAgeRung::Stale
                    && e.label.as_deref() == Some("this_test_own_span")),
            "expected a Stale emission naming this test's own span despite an older, \
             unrelated concurrent registration, got: {emissions:?}"
        );
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
            true,
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
            true,
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
            true,
        ));

        tokio::time::sleep(Duration::from_millis(40)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");
    }

    // Fix: task-level regressions
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
            true,
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
            true,
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
            true,
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

    // ── ADR-091 Amendment 2: Plank A (session sweep), Plank B (walpin
    // sidecar), Plank C (pin-depth probe) ────────────────────────────────

    #[tokio::test]
    async fn session_sweep_task_exits_on_shutdown_signal() {
        let cfg = SessionSweepConfig {
            interval: Duration::from_millis(10),
            ..SessionSweepConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_session_sweep_task(Vec::new(), cfg, shutdown_rx));

        shutdown_tx.send(()).expect("send shutdown signal");

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("session sweep task should exit within 1s")
            .expect("session sweep task panicked");
    }

    /// Bounded condition poll for filesystem effects of the async sweep
    /// task — fixed sleeps flake under parallel test load because sidecar
    /// writes fsync.
    async fn wait_for(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cond()
    }

    #[tokio::test]
    #[serial(khive_walpin_sidecar_env)]
    async fn walpin_observe_drops_beacon_when_heartbeat_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("observe_gate.db");
        let sidecar_dir = crate::walpin::sidecar_dir_for(&db_path);
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");

        let mut state = WalpinSidecarState::new(
            Some(db_path.as_path()),
            true,
            "session",
            Duration::from_millis(500),
        )
        .expect("sidecar enabled for a file-backed path");
        state.register_beacon().await;
        let pid = std::process::id();
        let beacon_path = sidecar_dir.join(format!("{pid}.beacon"));
        let before = std::fs::metadata(&beacon_path)
            .expect("beacon registered")
            .modified()
            .unwrap();

        // Force the heartbeat write to fail without touching directory
        // permissions (which would confound with the dir-mode validation):
        // occupy the exclusive-create temp name with a directory, so the
        // tolerant unlink and the O_EXCL create both fail.
        let obstruction = sidecar_dir.join(format!(".{pid}.json.tmp"));
        std::fs::create_dir(&obstruction).unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        let over_threshold = Some(khive_storage::tx_registry::OldestSpan {
            id: khive_storage::tx_registry::TxId(1),
            age: Duration::from_secs(60),
            label: None,
            origin: khive_storage::tx_registry::TxOrigin::Unscoped,
        });
        state
            .observe(over_threshold.clone(), Duration::from_secs(30))
            .await;

        assert!(
            !sidecar_dir.join(format!("{pid}.json")).exists(),
            "heartbeat write must have failed"
        );
        // Skipping the refresh alone would leave `before` fresh inside the
        // three-tick window; the fail-closed contract removes the beacon.
        assert!(
            !beacon_path.exists(),
            "a failed heartbeat write must remove the beacon — a still-fresh \
             beacon with no heartbeat would classify registered-silent \
             (before-mtime {before:?})"
        );

        // Recovery: clear the obstruction; the next over-threshold tick
        // writes the heartbeat and re-registers the beacon.
        std::fs::remove_dir(&obstruction).unwrap();
        state.observe(over_threshold, Duration::from_secs(30)).await;
        assert!(
            sidecar_dir.join(format!("{pid}.json")).exists(),
            "heartbeat must land once the write path recovers"
        );
        assert!(
            beacon_path.exists(),
            "beacon must re-register on the first healthy tick after removal"
        );
    }

    #[tokio::test]
    #[serial(tx_registry, khive_walpin_sidecar_env)]
    async fn session_sweep_task_writes_and_clears_walpin_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session_sweep.db");
        let pool = file_pool(&db_path);
        let sidecar_dir =
            crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed pool"));
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");

        let cfg = SessionSweepConfig {
            interval: Duration::from_millis(10),
            tx_warn_secs: Duration::from_millis(20),
            tx_max_age_secs: Duration::from_millis(500),
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_session_sweep_task(
            vec![SweepBackend {
                pool: Arc::clone(&pool),
                is_main: true,
            }],
            cfg,
            shutdown_rx,
        ));

        // No open span yet: a quiet process must write no *heartbeat*, but
        // it DOES register its one-time beacon at startup (ADR-091
        // Amendment 2 sidecar-health attribution) — the sidecar dir is not
        // empty, only heartbeat-free. Poll-wait rather than a fixed sleep:
        // the first tick fsyncs the beacon, and under parallel test load
        // that write can take longer than any small fixed window.
        let pid = std::process::id();
        let beacon = crate::walpin::beacon_path(&sidecar_dir, pid);
        assert!(
            wait_for(Duration::from_secs(2), || beacon.exists()).await,
            "a quiet process must still register its one-time beacon"
        );
        assert!(
            !sidecar_dir.join(format!("{pid}.json")).exists(),
            "a quiet process must not write a walpin heartbeat"
        );

        let tx_handle =
            khive_storage::tx_registry::register(Some("session_sweep_walpin_test".to_string()));
        let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
        assert!(
            wait_for(Duration::from_secs(2), || heartbeat_path.exists()).await,
            "expected a walpin heartbeat once the span crossed tx_warn_secs"
        );
        let body = std::fs::read_to_string(&heartbeat_path).unwrap();
        let hb: crate::walpin::WalpinHeartbeat = serde_json::from_str(&body).unwrap();
        assert_eq!(hb.pid, pid);
        assert_eq!(hb.process_role, "session");
        assert_eq!(
            hb.oldest_tx_label.as_deref(),
            Some("session_sweep_walpin_test")
        );
        assert_eq!(
            hb.attribution_basis.as_deref(),
            Some("fallback"),
            "an Unscoped span observed only through the main view's fallback \
             must carry attribution_basis=\"fallback\", never \"origin\""
        );

        drop(tx_handle);
        assert!(
            wait_for(Duration::from_secs(2), || !heartbeat_path.exists()).await,
            "heartbeat must be removed once the stale span clears"
        );

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("session sweep task should exit within 1s")
            .expect("session sweep task panicked");
    }

    /// ADR-091 Amendment 3 fan-out: two file-backed pools in one process,
    /// each its own `SweepBackend`. A span scoped to the SECONDARY pool's
    /// own origin must produce a heartbeat only in the secondary's sidecar
    /// — never the main backend's — and, because a `Secondary` filter never
    /// falls back to `Unscoped`, its heartbeat carries the evidence-backed
    /// `attribution_basis="origin"`. Uses the `graph_traverse_read` label
    /// (`stores/graph.rs`'s `traverse`) — the design note's own example of
    /// "the most WAL-pin-relevant span in the store" — as the registered
    /// span's label, so this doubles as coverage that a traversal read span
    /// surfaces correctly in a secondary backend's filtered view.
    #[tokio::test]
    #[serial(tx_registry, khive_walpin_sidecar_env)]
    async fn session_sweep_fan_out_scopes_secondary_span_to_secondary_sidecar_only() {
        let main_dir = tempfile::tempdir().unwrap();
        let secondary_dir = tempfile::tempdir().unwrap();
        let main_pool = file_pool(&main_dir.path().join("main.db"));
        let secondary_pool = file_pool(&secondary_dir.path().join("secondary.db"));
        let main_sidecar =
            crate::walpin::sidecar_dir_for(main_pool.canonical_path().expect("file-backed"));
        let secondary_sidecar =
            crate::walpin::sidecar_dir_for(secondary_pool.canonical_path().expect("file-backed"));
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");

        let cfg = SessionSweepConfig {
            interval: Duration::from_millis(10),
            tx_warn_secs: Duration::from_millis(20),
            tx_max_age_secs: Duration::from_millis(500),
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_session_sweep_task(
            vec![
                SweepBackend {
                    pool: Arc::clone(&main_pool),
                    is_main: true,
                },
                SweepBackend {
                    pool: Arc::clone(&secondary_pool),
                    is_main: false,
                },
            ],
            cfg,
            shutdown_rx,
        ));

        let pid = std::process::id();
        let secondary_heartbeat = secondary_sidecar.join(format!("{pid}.json"));
        let main_heartbeat = main_sidecar.join(format!("{pid}.json"));

        let tx_handle = khive_storage::tx_registry::register_scoped(
            Some("graph_traverse_read".to_string()),
            secondary_pool.origin(),
        );
        assert!(
            wait_for(Duration::from_secs(2), || secondary_heartbeat.exists()).await,
            "expected a walpin heartbeat in the secondary backend's own sidecar"
        );
        assert!(
            !main_heartbeat.exists(),
            "a span scoped to the secondary backend's origin must never produce \
             a heartbeat in the main backend's sidecar"
        );

        let body = std::fs::read_to_string(&secondary_heartbeat).unwrap();
        let hb: crate::walpin::WalpinHeartbeat = serde_json::from_str(&body).unwrap();
        assert_eq!(hb.oldest_tx_label.as_deref(), Some("graph_traverse_read"));
        assert_eq!(
            hb.attribution_basis.as_deref(),
            Some("origin"),
            "a Secondary-view winner is always Database-origin-backed — never fallback"
        );

        drop(tx_handle);
        assert!(
            wait_for(Duration::from_secs(2), || !secondary_heartbeat.exists()).await,
            "secondary heartbeat must be removed once its span clears"
        );
        assert!(
            !main_heartbeat.exists(),
            "the main sidecar must have stayed untouched for the whole tick sequence"
        );

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("session sweep task should exit within 1s")
            .expect("session sweep task panicked");
    }

    #[test]
    fn wal_pin_depth_arithmetic_against_real_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin_depth.db");
        let pool = file_pool(&path);
        let writer = pool.try_writer().expect("acquire writer");
        let conn = writer.conn();

        conn.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        conn.execute_batch("INSERT INTO t (v) VALUES (1)").unwrap();

        let (log, checkpointed) =
            query_wal_pin_depth(conn).expect("PRAGMA wal_checkpoint(PASSIVE) must succeed");
        // Nothing pins the WAL open in this test (no concurrent reader), so a
        // PASSIVE checkpoint must fully drain what it just wrote: pin depth
        // (log - checkpointed) is zero.
        assert!(
            log >= checkpointed,
            "checkpointed frames cannot exceed log frames"
        );
        assert_eq!(
            log - checkpointed,
            0,
            "an unpinned WAL must fully checkpoint under PASSIVE"
        );
    }

    #[test]
    fn wal_pin_depth_arithmetic_on_in_memory_pool_errors_cleanly() {
        // In-memory databases report `log = -1` (no WAL); the pragma read
        // itself does not panic and the caller (`log_wal_pin_depth`) treats
        // any error as a logged warning, never a crash.
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("in-memory pool");
        let writer = pool.try_writer().expect("acquire writer");
        // Either an explicit error or a nonsensical negative `log` value is
        // acceptable here — the requirement is just "does not panic".
        let _ = query_wal_pin_depth(writer.conn());
    }
}
