//! Periodic WAL checkpoint task for the connection pool (ADR-091; dedicated
//! checkpoint connection amendment, see below).
//!
//! Issues `PRAGMA wal_checkpoint(PASSIVE)` on every tick — non-blocking, never
//! waits for readers. A rare, separately-gated escalation may additionally run
//! `PRAGMA wal_checkpoint(TRUNCATE)` once WAL pressure crosses
//! `truncate_high_water_pages` and `truncate_min_interval` has elapsed since
//! the last attempt (Plank 2); both run on the task's own dedicated
//! standalone connection (`CheckpointConnection`), opened once at task
//! startup and reused for every tick — `checkpoint_once` never checks out the
//! pool's writer mutex at all, so a concurrent `pool.writer()` checkout can
//! never queue behind a checkpoint tick's ADMISSION. That guarantee is
//! admission-only: PASSIVE takes SQLite's CKPT lock, not the WRITE lock, so
//! it never blocks writers at the SQLite level either — but TRUNCATE
//! additionally acquires SQLite's writer lock and can still block a
//! concurrent write transaction, on any connection, for up to
//! `truncate_busy_timeout` while it waits on a pinning reader, exactly as
//! before this connection split.
//!
//! If the dedicated connection is unavailable (never opened yet, or dropped
//! after a prior tick's connection-level pragma failure), the tick reports
//! `CheckpointTick::Skipped` and the next tick lazily reopens it — this is
//! now the ONLY source of a Skipped tick; a busy pool writer no longer causes
//! one.
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
//! from ordinary ticks, the dedicated-connection invariant, and why Plank 1
//! is a sweep rather than the ADR's originally-described per-statement guard).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pool::ConnectionPool;

// ── metrics read-surface (load/perf harness) ─────────────────────────────
// Read-only process-wide gauges (never reset outside #[cfg(test)]). See
// crates/khive-db/docs/api/checkpoint.md#metrics-read-surface-loadperf-harness

/// Last-observed WAL page count (the routine PASSIVE row's `log` value, or a
/// rare post-TRUNCATE observation from `maybe_truncate`).
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

/// Count of checkpoint ticks skipped because the task's dedicated
/// `CheckpointConnection` was unavailable that tick (never opened yet, or
/// dropped after a prior connection-level pragma failure — ADR-091
/// checkpoint-pressure telemetry), across this process's lifetime. Never
/// reset outside `#[cfg(test)]`.
static CHECKPOINT_SKIPPED_TICKS: AtomicU64 = AtomicU64::new(0);

/// Current run-length of consecutive skipped ticks. Reset to 0 the next time
/// a tick is actually observed (dedicated connection available), so a
/// sustained skip streak is visible even between two successful
/// observations.
static CHECKPOINT_CONSECUTIVE_SKIPS: AtomicU64 = AtomicU64::new(0);

/// WAL page count as of the most recent *observed* tick, snapshotted at the
/// moment a skip occurs. `u64::MAX` is the "no skip has recorded a snapshot
/// yet" sentinel, mirroring `LAST_WAL_PAGES`.
static CHECKPOINT_LAST_SKIP_WAL_PAGES: AtomicU64 = AtomicU64::new(u64::MAX);

/// Elevated checkpoint observations aggregated in memory instead of written
/// as one primary-store lifecycle row per tick (#1838).
static CHECKPOINT_PRESSURE_ELEVATED_TICKS: AtomicU64 = AtomicU64::new(0);

/// Below-to-above `warn_pages` transitions observed by checkpoint tasks.
static CHECKPOINT_PRESSURE_EPISODES_STARTED: AtomicU64 = AtomicU64::new(0);

/// Above-to-below `warn_pages` transitions observed by checkpoint tasks.
static CHECKPOINT_PRESSURE_EPISODES_RECOVERED: AtomicU64 = AtomicU64::new(0);

/// Primary-store append calls actually made by checkpoint lifecycle workers.
static CHECKPOINT_LIFECYCLE_APPEND_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Checkpoint lifecycle append calls that returned a storage error.
static CHECKPOINT_LIFECYCLE_APPEND_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Lifecycle transitions rejected before append because the bounded handoff
/// was full, closed, or could not serialize the payload.
static CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS: AtomicU64 = AtomicU64::new(0);

/// Count of cached-reader explicit read transactions rolled back on reuse
/// for exceeding `read_tx_max_age` (#1846), across this process's lifetime.
/// Unlike the Plank 1 sweep above, this is reclamation, not just visibility:
/// each count here is a WAL snapshot that was actually released rather than
/// merely logged as stale. See `sql_bridge.rs::execute_standalone_read`.
static READ_TX_MAX_AGE_EVICTIONS: AtomicU64 = AtomicU64::new(0);

/// One backend-scoped observation produced by the periodic checkpoint task's
/// own PASSIVE pass. Logical frame counts and the physical `-wal` allocation
/// are intentionally separate: SQLite may retain/reuse the sidecar after the
/// logical backlog drains (#1849).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineWalObservation {
    pub busy: i64,
    pub log_frames: u64,
    pub checkpointed_frames: u64,
    pub pending_frames: u64,
    pub physical_wal_bytes: Option<u64>,
    pub observed_at_unix_ms: u64,
}

/// Latest routine observation by canonical database identity. Checkpoint
/// tasks fan out per backend, so a single process-global "last task wins"
/// gauge would misattribute a secondary backend to the main metrics frame.
static ROUTINE_WAL_OBSERVATIONS: OnceLock<Mutex<HashMap<Option<PathBuf>, RoutineWalObservation>>> =
    OnceLock::new();

fn routine_wal_observations() -> &'static Mutex<HashMap<Option<PathBuf>, RoutineWalObservation>> {
    ROUTINE_WAL_OBSERVATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn checkpoint_db_key_from_path(path: Option<&Path>) -> Option<PathBuf> {
    path.map(Path::to_path_buf)
}

fn checkpoint_db_key(pool: &ConnectionPool) -> Option<PathBuf> {
    checkpoint_db_key_from_path(pool.canonical_path())
}

fn observed_at_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn physical_wal_bytes(pool: &ConnectionPool) -> Option<u64> {
    let path = pool.canonical_path()?;
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push("-wal");
    std::fs::metadata(PathBuf::from(sidecar))
        .ok()
        .map(|metadata| metadata.len())
}

fn record_routine_wal_observation(
    pool: &ConnectionPool,
    raw: RawCheckpointObservation,
) -> RoutineWalObservation {
    let log_frames = raw.log_frames.max(0) as u64;
    let checkpointed_frames = raw.checkpointed_frames.max(0) as u64;
    let observation = RoutineWalObservation {
        busy: raw.busy,
        log_frames,
        checkpointed_frames,
        pending_frames: log_frames.saturating_sub(checkpointed_frames),
        physical_wal_bytes: physical_wal_bytes(pool),
        observed_at_unix_ms: observed_at_unix_ms(),
    };
    routine_wal_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(checkpoint_db_key(pool), observation.clone());
    observation
}

/// Latest periodic checkpoint sample for this exact backend. This is a pure
/// in-memory read: it never issues `wal_checkpoint` or stats the filesystem.
pub fn routine_wal_observation(pool: &ConnectionPool) -> Option<RoutineWalObservation> {
    routine_wal_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&checkpoint_db_key(pool))
        .cloned()
}

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

/// Total checkpoint ticks skipped (dedicated connection unavailable) in this
/// process's lifetime.
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

/// Total at/above-`warn_pages` observations aggregated in memory.
pub fn checkpoint_pressure_elevated_ticks() -> u64 {
    CHECKPOINT_PRESSURE_ELEVATED_TICKS.load(Ordering::Relaxed)
}

/// Total pressure episodes observed to start in this process.
pub fn checkpoint_pressure_episodes_started() -> u64 {
    CHECKPOINT_PRESSURE_EPISODES_STARTED.load(Ordering::Relaxed)
}

/// Total pressure episodes observed to recover in this process.
pub fn checkpoint_pressure_episodes_recovered() -> u64 {
    CHECKPOINT_PRESSURE_EPISODES_RECOVERED.load(Ordering::Relaxed)
}

/// Total primary-store append calls made by checkpoint lifecycle workers.
pub fn checkpoint_lifecycle_append_attempts() -> u64 {
    CHECKPOINT_LIFECYCLE_APPEND_ATTEMPTS.load(Ordering::Relaxed)
}

/// Total checkpoint lifecycle append calls that returned a storage error.
pub fn checkpoint_lifecycle_append_failures() -> u64 {
    CHECKPOINT_LIFECYCLE_APPEND_FAILURES.load(Ordering::Relaxed)
}

/// Total cached-reader read transactions rolled back on reuse for exceeding
/// `read_tx_max_age` (#1846), across this process's lifetime.
pub fn read_tx_max_age_evictions() -> u64 {
    READ_TX_MAX_AGE_EVICTIONS.load(Ordering::Relaxed)
}

/// Records one cached-reader read transaction rolled back on reuse for
/// exceeding `read_tx_max_age`. Called from `sql_bridge.rs` at the point the
/// rollback is issued, regardless of whether the rollback itself succeeds —
/// this counts the eviction *attempt*, matching the `truncate_attempts`
/// naming convention above.
pub(crate) fn note_read_tx_max_age_eviction() {
    READ_TX_MAX_AGE_EVICTIONS.fetch_add(1, Ordering::Relaxed);
}

/// Total checkpoint lifecycle transitions rejected before append.
pub fn checkpoint_lifecycle_enqueue_drops() -> u64 {
    CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.load(Ordering::Relaxed)
}

/// A tick's dedicated checkpoint connection was unavailable: bump the
/// lifetime and consecutive-skip counters and snapshot the last-known WAL
/// pressure so an operator can see how bad the WAL was heading into the skip
/// streak.
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

fn note_checkpoint_pressure_observation(above_warn: bool, was_above_warn: bool) {
    if above_warn {
        CHECKPOINT_PRESSURE_ELEVATED_TICKS.fetch_add(1, Ordering::Relaxed);
        if !was_above_warn {
            CHECKPOINT_PRESSURE_EPISODES_STARTED.fetch_add(1, Ordering::Relaxed);
        }
    } else if was_above_warn {
        CHECKPOINT_PRESSURE_EPISODES_RECOVERED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reset the checkpoint-pressure atomics between tests. Process-wide gauges
/// are otherwise shared across every test in this binary; tests that assert
/// on them must reset first and run under a shared `#[serial(...)]` group.
#[cfg(test)]
pub(crate) fn reset_checkpoint_metrics_for_tests() {
    CHECKPOINT_SKIPPED_TICKS.store(0, Ordering::Relaxed);
    CHECKPOINT_CONSECUTIVE_SKIPS.store(0, Ordering::Relaxed);
    CHECKPOINT_LAST_SKIP_WAL_PAGES.store(u64::MAX, Ordering::Relaxed);
    CHECKPOINT_PRESSURE_ELEVATED_TICKS.store(0, Ordering::Relaxed);
    CHECKPOINT_PRESSURE_EPISODES_STARTED.store(0, Ordering::Relaxed);
    CHECKPOINT_PRESSURE_EPISODES_RECOVERED.store(0, Ordering::Relaxed);
    CHECKPOINT_LIFECYCLE_APPEND_ATTEMPTS.store(0, Ordering::Relaxed);
    CHECKPOINT_LIFECYCLE_APPEND_FAILURES.store(0, Ordering::Relaxed);
    CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.store(0, Ordering::Relaxed);
    READ_TX_MAX_AGE_EVICTIONS.store(0, Ordering::Relaxed);
}

/// Outcome of a single checkpoint attempt.
///
/// `Skipped` is returned when the task's dedicated `CheckpointConnection`
/// is unavailable that tick (the tick is a no-op) — never because a
/// concurrent pool writer was busy; a checkpoint tick no longer checks out
/// the pool's writer mutex at all. `Observed` carries the WAL page count read
/// during the tick. The distinction matters for threshold-crossing WARN
/// rate-limiting: a skipped tick must leave the above/below state unchanged
/// so that it cannot spuriously re-arm the rate limit while WAL pressure is
/// still elevated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTick {
    /// The dedicated checkpoint connection was unavailable; no checkpoint
    /// was issued this tick.
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
    /// A skipped tick (dedicated connection unavailable, below threshold, or
    /// interval not yet elapsed) never advances the "last attempt" clock, so
    /// the next tick where the connection is available and the threshold is
    /// still crossed is immediately eligible rather than waiting out the
    /// full interval again.
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
    /// oldest registry entry to `tracing::error!`. The sweep itself is
    /// visibility only — nothing in `TxAgeSweepState` force-closes a stale
    /// span. `sql_bridge.rs`'s cached-reader read-transaction path shares
    /// this exact value (via `PoolConfig::read_tx_max_age`, #1846) to
    /// actually roll back and evict an explicit read transaction the next
    /// time its handle is reused past this age — reclamation for the
    /// "reused periodically" case, not the "held idle with no further calls"
    /// case the ADR named as its accepted gap; see
    /// `crates/khive-db/docs/api/checkpoint.md`'s Plank 1 section for the
    /// distinction and why the latter remains open design work.
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
pub(crate) fn tx_age_thresholds_from_env(
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

#[cfg(unix)]
const DEFAULT_WALPIN_FULL_SCAN_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(unix)]
#[derive(Debug, Clone)]
struct CachedWalpinAttribution {
    report: crate::walpin::WalpinReport,
    census: Result<crate::walpin::CensusResult, String>,
    captured_at: Instant,
}

#[cfg(unix)]
#[derive(Debug)]
enum WalpinFullScanPlan {
    Refresh {
        previous_last_attempt: Option<Instant>,
    },
    Cached(CachedWalpinAttribution),
    Suppressed,
}

/// Mutable escalation state carried across ticks by the caller (ADR-091 Plank 2).
///
/// Kept separate from [`CheckpointConfig`] because it is *state*, not
/// configuration: `last_attempt` and `consecutive_failures` mutate every tick,
/// while `CheckpointConfig` is parsed once and held immutable for the life of
/// the task.
#[derive(Debug)]
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
    /// Fallback freshness cadence for legacy sidecar records that do not
    /// declare their producer interval. Captured once when the daemon task
    /// starts; this is ADR-091's compiled 5000 ms session-sweep default, never
    /// the daemon's faster checkpoint cadence or a local environment override.
    #[cfg(unix)]
    legacy_walpin_fallback_interval: Duration,
    /// Minimum spacing between full sidecar/OS-holder enumeration attempts.
    /// The attempt timestamp advances before blocking work starts, so an I/O
    /// failure or worker panic cannot turn sustained pressure into a hot retry
    /// loop. A successful report is retained only for diagnostic reuse.
    #[cfg(unix)]
    walpin_full_scan_interval: Duration,
    #[cfg(unix)]
    walpin_full_scan_last_attempt: Option<Instant>,
    #[cfg(unix)]
    walpin_cached_attribution: Option<CachedWalpinAttribution>,
    /// Whether the no-progress attribution arm already attempted the one
    /// bounded sidecar enumeration allowed for this checkpoint tick.
    #[cfg(unix)]
    sidecar_attribution_attempted_this_tick: bool,
}

impl Default for TruncateState {
    fn default() -> Self {
        Self {
            last_attempt: None,
            consecutive_failures: 0,
            #[cfg(unix)]
            legacy_walpin_fallback_interval: DEFAULT_SESSION_SWEEP_INTERVAL,
            #[cfg(unix)]
            walpin_full_scan_interval: DEFAULT_WALPIN_FULL_SCAN_INTERVAL,
            #[cfg(unix)]
            walpin_full_scan_last_attempt: None,
            #[cfg(unix)]
            walpin_cached_attribution: None,
            #[cfg(unix)]
            sidecar_attribution_attempted_this_tick: false,
        }
    }
}

impl TruncateState {
    #[cfg(unix)]
    fn with_legacy_walpin_fallback(interval: Duration) -> Self {
        Self {
            legacy_walpin_fallback_interval: interval,
            ..Self::default()
        }
    }

    #[cfg(all(test, unix))]
    fn with_walpin_full_scan_cadence(interval: Duration) -> Self {
        Self {
            walpin_full_scan_interval: interval,
            ..Self::default()
        }
    }

    #[cfg(unix)]
    fn begin_tick(&mut self) {
        self.sidecar_attribution_attempted_this_tick = false;
    }

    #[cfg(unix)]
    fn housekeeping_due(&self) -> bool {
        !self.sidecar_attribution_attempted_this_tick
            && self.walpin_full_scan_due_at(Instant::now())
    }

    #[cfg(unix)]
    fn walpin_full_scan_due_at(&self, now: Instant) -> bool {
        self.walpin_full_scan_last_attempt.is_none_or(|last| {
            now.saturating_duration_since(last) >= self.walpin_full_scan_interval
        })
    }

    #[cfg(unix)]
    fn claim_walpin_full_scan_at(&mut self, now: Instant) -> bool {
        if !self.walpin_full_scan_due_at(now) {
            return false;
        }
        self.walpin_full_scan_last_attempt = Some(now);
        true
    }

    #[cfg(unix)]
    fn plan_walpin_attribution_at(&mut self, now: Instant) -> WalpinFullScanPlan {
        if self.walpin_full_scan_due_at(now) {
            let previous_last_attempt = self.walpin_full_scan_last_attempt.replace(now);
            WalpinFullScanPlan::Refresh {
                previous_last_attempt,
            }
        } else if let Some(cached) = self.walpin_cached_attribution.clone() {
            WalpinFullScanPlan::Cached(cached)
        } else {
            WalpinFullScanPlan::Suppressed
        }
    }

    #[cfg(unix)]
    fn restore_walpin_full_scan_reservation(&mut self, previous_last_attempt: Option<Instant>) {
        self.walpin_full_scan_last_attempt = previous_last_attempt;
    }

    #[cfg(unix)]
    fn cache_walpin_attribution(
        &mut self,
        report: crate::walpin::WalpinReport,
        census: Result<crate::walpin::CensusResult, String>,
        captured_at: Instant,
    ) {
        self.walpin_cached_attribution = Some(CachedWalpinAttribution {
            report,
            census,
            captured_at,
        });
    }
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
/// or a session's `run_session_sweep_task`). Once the registry's oldest span
/// exceeds `tx_warn_secs`, the first observation and each content change
/// rewrite the heartbeat body; unchanged ticks refresh only its mtime. The
/// heartbeat is removed once when the condition clears (and on shutdown), so
/// a process that never crosses the threshold writes no heartbeat body.
struct WalpinSidecarState {
    dir: PathBuf,
    pid: u32,
    role: &'static str,
    started_at: i64,
    /// This sweep's own tick cadence, recorded into every beacon and
    /// heartbeat so the enumerating daemon judges freshness against the
    /// PRODUCER's interval — a session on an independently slower configured
    /// cadence must not be misread as stale.
    sweep_interval_ms: u64,
    wrote: bool,
    /// Whether this process's registration beacon is believed present on
    /// disk. Cleared when a failed heartbeat write escalates to beacon
    /// removal (fail-closed — see `observe`) or a beacon touch fails; the
    /// next healthy tick then re-registers with a full write instead of a
    /// metadata touch.
    beacon_registered: bool,
    /// The content actually on disk in the last successful heartbeat body
    /// write, if any (ADR-091 Amendment 3 Plank F1). `None` whenever the
    /// next tick must go through a full write — no heartbeat written yet,
    /// the last write failed, or the threshold cleared. Compared against
    /// each new observation to decide touch (content unchanged) vs.
    /// rewrite (content changed).
    last_heartbeat: Option<LastHeartbeatState>,
}

/// ADR-091 Amendment 3 Plank F1: the content signature of the heartbeat
/// body currently on disk, plus the `oldest_tx_started_at` value that body
/// carries — kept separate from the signature proper because it is derived
/// (fixed for as long as the same span stays oldest), not an independent
/// change signal.
struct LastHeartbeatState {
    span_id: khive_storage::tx_registry::TxId,
    label: Option<String>,
    attribution_basis: &'static str,
    sweep_interval_ms: u64,
    oldest_tx_started_at: i64,
}

impl LastHeartbeatState {
    /// Whether a fresh observation carries exactly the content already on
    /// disk — the licensing condition for a metadata-only touch instead of
    /// a full body rewrite (the first over-threshold observation, a change
    /// of the oldest span's identity or label, a change of
    /// `attribution_basis`, or a change of the declared sweep cadence).
    fn content_matches(
        &self,
        span_id: khive_storage::tx_registry::TxId,
        label: &Option<String>,
        attribution_basis: &str,
        sweep_interval_ms: u64,
    ) -> bool {
        self.span_id == span_id
            && self.label == *label
            && self.attribution_basis == attribution_basis
            && self.sweep_interval_ms == sweep_interval_ms
    }
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
            sweep_interval_ms: interval.as_millis().min(u64::MAX as u128) as u64,
            wrote: false,
            last_heartbeat: None,
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
            sweep_interval_ms: self.sweep_interval_ms,
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

    /// Run one bounded housekeeping pass independently of WAL pressure. The
    /// collector removes only positively dead/reused-PID residue; uncertain
    /// evidence remains for a no-progress attribution pass. Directory work
    /// and report memory are capped, and all blocking filesystem operations
    /// stay off the async runtime worker.
    #[cfg(unix)]
    async fn reap_dead_entries_bounded(
        &self,
        legacy_fallback_interval: Duration,
    ) -> Option<crate::walpin::WalpinReport> {
        let dir = self.dir.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::walpin::housekeep_live(&dir, legacy_fallback_interval)
        })
        .await;
        match result {
            Ok(Ok(report)) => Some(report),
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 6: bounded walpin sidecar cleanup failed"
                );
                None
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 6: walpin sidecar cleanup task panicked"
                );
                None
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

                // ADR-091 Amendment 3 Plank F1: a metadata-only mtime touch
                // advances freshness whenever nothing content-relevant has
                // changed since the last body write; a full rewrite happens
                // only on the first over-threshold observation or a genuine
                // content change.
                let content_unchanged = self.wrote
                    && self.last_heartbeat.as_ref().is_some_and(|last| {
                        last.content_matches(
                            span.id,
                            &span.label,
                            attribution_basis,
                            self.sweep_interval_ms,
                        )
                    });

                if content_unchanged {
                    let dir = self.dir.clone();
                    let pid = self.pid;
                    let touch_result = tokio::task::spawn_blocking(move || {
                        crate::walpin::touch_heartbeat(&dir, pid)
                    })
                    .await;
                    match touch_result {
                        Ok(Ok(())) => {
                            self.refresh_beacon().await;
                            return;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                error = %e,
                                "ADR-091 Amendment 3 Plank F1: walpin heartbeat touch failed; \
                                 recreating with a full body write"
                            );
                        }
                        Err(join_err) => {
                            tracing::warn!(
                                error = %join_err,
                                "ADR-091 Amendment 3 Plank F1: walpin heartbeat touch task \
                                 panicked; recreating with a full body write"
                            );
                        }
                    }
                    // Recovery rule: the touch path must never assume the
                    // target still exists — enumeration can delete a slow
                    // writer's heartbeat while its span is still live. Fall
                    // through to the full write below unconditionally.
                }

                // The oldest span's registration instant is fixed for as
                // long as it stays the SAME span: reuse the previously
                // recorded value rather than re-deriving it from `now -
                // age`, which would drift by measurement noise across ticks
                // for no reason. A genuinely new oldest span (or the first
                // observation) derives it fresh.
                let oldest_tx_started_at = self
                    .last_heartbeat
                    .as_ref()
                    .filter(|last| last.span_id == span.id)
                    .map(|last| last.oldest_tx_started_at)
                    .unwrap_or_else(|| now_epoch_secs().saturating_sub(span.age.as_secs() as i64));

                let heartbeat = crate::walpin::WalpinHeartbeat {
                    pid: self.pid,
                    process_role: self.role.to_string(),
                    started_at: self.started_at,
                    oldest_tx_age_secs: span.age.as_secs_f64(),
                    oldest_tx_label: span.label.clone(),
                    oldest_tx_started_at: Some(oldest_tx_started_at),
                    updated_at: now_epoch_secs(),
                    sweep_interval_ms: self.sweep_interval_ms,
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
                        self.last_heartbeat = Some(LastHeartbeatState {
                            span_id: span.id,
                            label: span.label,
                            attribution_basis,
                            sweep_interval_ms: self.sweep_interval_ms,
                            oldest_tx_started_at,
                        });
                        self.refresh_beacon().await;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            "ADR-091 Amendment 2 Plank B: failed to write walpin heartbeat; \
                             removing beacon so this process cannot read as \
                             registered-silent while over threshold"
                        );
                        // Unknown what (if anything) is on disk now — the
                        // next tick must go through a full write, never a
                        // touch, until a write actually lands.
                        self.last_heartbeat = None;
                        self.drop_beacon_fail_closed().await;
                    }
                    Err(join_err) => {
                        tracing::warn!(
                            error = %join_err,
                            "ADR-091 Amendment 2 Plank B: walpin heartbeat write task panicked"
                        );
                        self.last_heartbeat = None;
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
                    self.last_heartbeat = None;
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

#[cfg(unix)]
async fn run_walpin_housekeeping_if_due(
    sidecar: &WalpinSidecarState,
    state: &mut TruncateState,
    legacy_fallback_interval: Duration,
) -> bool {
    if !state.housekeeping_due() || !state.claim_walpin_full_scan_at(Instant::now()) {
        return false;
    }
    if let Some(report) = sidecar
        .reap_dead_entries_bounded(legacy_fallback_interval)
        .await
    {
        state.cache_walpin_attribution(
            report,
            Err("OS holder census is unavailable for a housekeeping-only scan".to_string()),
            Instant::now(),
        );
    }
    true
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
const DEFAULT_SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

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
            interval: DEFAULT_SESSION_SWEEP_INTERVAL,
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
        let mut cfg = Self {
            interval: session_sweep_interval_from_env(),
            ..Self::default()
        };
        // Shares `tx_age_thresholds_from_env` with `CheckpointConfig::from_env`
        // (minor, ADR-091 Amendment 2) so a session and the daemon
        // parse and validate `KHIVE_TX_WARN_SECS`/`KHIVE_TX_MAX_AGE_SECS`
        // identically from one source, not two hand-copied blocks.
        (cfg.tx_warn_secs, cfg.tx_max_age_secs) =
            tx_age_thresholds_from_env(cfg.tx_warn_secs, cfg.tx_max_age_secs);

        cfg
    }
}

fn session_sweep_interval_from_env() -> Duration {
    std::env::var("KHIVE_SESSION_SWEEP_INTERVAL_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_SESSION_SWEEP_INTERVAL)
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

/// The event sink and namespace owned by one checkpoint task in a fan-out.
///
/// Backend role and lifecycle ownership are separate: a secondary task may
/// own lifecycle emission when the deployment's main backend is in-memory.
#[derive(Clone)]
pub struct CheckpointLifecycleOwner {
    event_store: Arc<dyn khive_storage::EventStore>,
    namespace: String,
}

impl CheckpointLifecycleOwner {
    /// Designate `event_store` as the lifecycle sink for one checkpoint task.
    pub fn new(
        event_store: Arc<dyn khive_storage::EventStore>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            event_store,
            namespace: namespace.into(),
        }
    }
}

/// Maximum number of checkpoint lifecycle events waiting behind the append
/// currently owned by the worker. One queued row preserves a recent outcome
/// without allowing sustained writer contention to grow memory without bound.
const CHECKPOINT_LIFECYCLE_QUEUE_CAPACITY: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckpointPressureEpisode {
    elevated_ticks: u64,
    peak_wal_pages: u64,
}

impl CheckpointPressureEpisode {
    fn start(wal_pages: u64) -> Self {
        Self {
            elevated_ticks: 1,
            peak_wal_pages: wal_pages,
        }
    }

    fn observe(&mut self, wal_pages: u64) {
        self.elevated_ticks = self.elevated_ticks.saturating_add(1);
        self.peak_wal_pages = self.peak_wal_pages.max(wal_pages);
    }
}

/// Zero-wait handoff from the checkpoint scheduler to its lifecycle sink.
///
/// The worker serializes appends, preserving the order of every event that is
/// accepted. The scheduler only calls [`tokio::sync::mpsc::Sender::try_send`]:
/// if the worker and its single queue slot are both occupied, telemetry is
/// dropped rather than delaying the next checkpoint cycle. The first drop in
/// each uninterrupted full-queue episode warns; a successful enqueue re-arms
/// that warning without producing per-tick log spam.
struct CheckpointLifecycleEmitter {
    namespace: Option<String>,
    sender: Option<tokio::sync::mpsc::Sender<khive_storage::Event>>,
    worker: Option<tokio::task::JoinHandle<()>>,
    busy_warning_emitted: bool,
}

impl CheckpointLifecycleEmitter {
    fn new(owner: Option<CheckpointLifecycleOwner>) -> Self {
        let Some(owner) = owner else {
            return Self {
                namespace: None,
                sender: None,
                worker: None,
                busy_warning_emitted: false,
            };
        };

        let namespace = owner.namespace.clone();
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<khive_storage::Event>(CHECKPOINT_LIFECYCLE_QUEUE_CAPACITY);
        let worker = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let kind = event.kind;
                CHECKPOINT_LIFECYCLE_APPEND_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
                if let Err(err) = owner.event_store.append_event(event).await {
                    CHECKPOINT_LIFECYCLE_APPEND_FAILURES.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        error = %err,
                        event_kind = %kind.name(),
                        "checkpoint lifecycle event append failed"
                    );
                }
            }
        });

        Self {
            namespace: Some(namespace),
            sender: Some(sender),
            worker: Some(worker),
            busy_warning_emitted: false,
        }
    }

    /// Serialize and enqueue one lifecycle event without awaiting sink I/O.
    /// Returns whether the row was accepted for delivery (or no sink exists).
    fn try_emit<P: serde::Serialize>(&mut self, kind: khive_types::EventKind, payload: P) -> bool {
        let (Some(namespace), Some(sender)) = (&self.namespace, &self.sender) else {
            return true;
        };
        let payload_value = match serde_json::to_value(&payload) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    event_kind = %kind.name(),
                    "failed to serialize checkpoint lifecycle event payload"
                );
                CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        let payload_schema_version = match kind {
            khive_types::EventKind::CheckpointOutcomeRecorded => 2,
            _ => 1,
        };
        let event = khive_storage::Event::new(
            namespace,
            "checkpoint.lifecycle",
            kind,
            khive_types::SubstrateKind::Event,
            "daemon:checkpoint_task",
        )
        .with_payload(payload_value)
        .with_payload_schema_version(payload_schema_version);

        match sender.try_send(event) {
            Ok(()) => {
                self.busy_warning_emitted = false;
                true
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
                if !self.busy_warning_emitted {
                    tracing::warn!(
                        event_kind = %event.kind.name(),
                        queue_capacity = CHECKPOINT_LIFECYCLE_QUEUE_CAPACITY,
                        "checkpoint lifecycle event dropped because the append worker is busy"
                    );
                    self.busy_warning_emitted = true;
                }
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(event)) => {
                CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    event_kind = %event.kind.name(),
                    "checkpoint lifecycle event dropped because the append worker stopped"
                );
                false
            }
        }
    }

    /// Stop the scheduler-owned async worker without making
    /// [`run_checkpoint_task`] wait for its current append future.
    ///
    /// This bounds checkpoint-task shutdown only. If the event store already
    /// admitted the append to `spawn_blocking` or a `WriterTask`, aborting this
    /// worker cannot cancel that downstream operation; at most one such sink
    /// operation may outlive the checkpoint task.
    async fn shutdown(mut self) {
        drop(self.sender.take());
        let Some(worker) = self.worker.take() else {
            return;
        };
        worker.abort();
        match worker.await {
            Ok(()) => {}
            Err(err) if err.is_cancelled() => {}
            Err(err) => tracing::warn!(
                error = %err,
                "checkpoint lifecycle event append worker terminated unexpectedly"
            ),
        }
    }
}

impl Drop for CheckpointLifecycleEmitter {
    fn drop(&mut self) {
        // The normal watch-signal path calls `shutdown` and takes the handle
        // first. This fallback covers an externally-aborted or panicking
        // checkpoint task so the scheduler-owned async worker itself is never
        // detached. One already-admitted downstream sink operation may outlive
        // it; see `shutdown`'s contract above.
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

/// The checkpoint task's dedicated, long-lived standalone connection to the
/// same database file — opened once at task startup and reused for every
/// tick's PASSIVE (and, when armed, TRUNCATE) pragma. NEVER the pool's writer
/// mutex, which is what removes the pool-mutex ADMISSION path: a concurrent
/// `pool.writer()` checkout no longer queues behind a checkpoint tick.
///
/// That removal is scoped to admission, not to SQLite-level blocking in
/// general. `PRAGMA wal_checkpoint(PASSIVE)` takes only SQLite's CKPT lock,
/// not the WRITE lock, so a concurrent writer can commit while a PASSIVE pass
/// runs on this connection — true of PASSIVE specifically, not of TRUNCATE.
/// TRUNCATE inherits RESTART semantics and additionally acquires SQLite's
/// writer lock, so it can still block a concurrent write transaction, on any
/// connection, for up to `truncate_busy_timeout` while it waits on a pinning
/// reader — the same bounded cost that existed pre-fix, now paid on this
/// dedicated connection instead of the pool writer. Serializing checkpoint
/// admission behind the pool's writer mutex (the pre-fix design) imposed
/// contention SQLite itself does not require; TRUNCATE's own SQLite-level
/// write-blocking window is unaffected by that removal.
///
/// `None` between ticks means the connection is unavailable (never opened
/// yet, or dropped after a prior tick's connection-level pragma failure) —
/// the caller must report that tick `Skipped` and retry the open on the next
/// one. This is now the ONLY source of a `Skipped` tick.
///
/// ADR-136 D1 gate 5 classification: **checkpoint writer**. Explicitly
/// exempt from `WriterTask`/queue routing by design (see the admission-path
/// note above), never `SqlAccess`-reachable, never counted as a
/// `direct_route_violation` — see the classification table in
/// `writer_task`'s module doc.
struct CheckpointConnection {
    conn: Option<rusqlite::Connection>,
    /// Consecutive failed `open_standalone_writer` attempts since the last
    /// successful open (or since task startup). Drives the WARN-once /
    /// debug-thereafter log rate-limiting in `ensure_open`: a file-backed
    /// pool that transiently loses its dedicated connection would otherwise
    /// log a WARN on every tick (default 500ms) for as long as the outage
    /// lasts, which for a read-only or in-memory pool — where the open can
    /// never succeed — means permanent per-tick WARN spam.
    consecutive_open_failures: u32,
}

impl CheckpointConnection {
    fn new() -> Self {
        Self {
            conn: None,
            consecutive_open_failures: 0,
        }
    }

    /// Ensure a usable connection is open, lazily (re)opening from `pool`
    /// when the current one is absent. Reuses the crate's existing untracked
    /// standalone-connection open path (`ConnectionPool::open_standalone_writer_untracked`),
    /// which applies the same pragmas (including `busy_timeout` from the pool
    /// config) as any other standalone connection, without counting this
    /// infrastructure connection as write-operation traffic. Returns `None` if
    /// opening fails — an in-memory pool (no on-disk file to open a second
    /// connection against), a read-only pool, or a transient filesystem error.
    ///
    /// Logging is rate-limited across a failure streak: the FIRST failure of
    /// a streak logs at `warn!`, every subsequent identical failure (while
    /// still failing) logs at `debug!` instead, and a successful open that
    /// ends a streak logs one `info!` recovery line. Without this, a
    /// permanently-unopenable pool (read-only or in-memory, selected by
    /// `checkpoint_pool_for`) would WARN on every tick forever.
    fn ensure_open(&mut self, pool: &ConnectionPool) -> Option<&rusqlite::Connection> {
        if self.conn.is_none() {
            match pool.open_standalone_writer_untracked() {
                Ok(conn) => {
                    // This is the dedicated owner's own connection: disable
                    // autocheckpoint on it unconditionally, independent of
                    // whether the pool-level ownership claim has landed yet
                    // (the standalone open applies the claim-dependent
                    // value; this connection must never run an implicit
                    // checkpoint inside its own PASSIVE/TRUNCATE work).
                    if let Err(e) = conn.pragma_update(None, "wal_autocheckpoint", 0) {
                        tracing::warn!(
                            error = %e,
                            "could not disable autocheckpoint on the dedicated checkpoint \
                             connection"
                        );
                    }
                    if self.consecutive_open_failures > 0 {
                        tracing::info!(
                            prior_consecutive_failures = self.consecutive_open_failures,
                            "dedicated checkpoint connection opened successfully, ending a \
                             failure streak"
                        );
                    }
                    self.consecutive_open_failures = 0;
                    self.conn = Some(conn);
                }
                Err(e) => {
                    if self.consecutive_open_failures == 0 {
                        tracing::warn!(
                            error = %e,
                            "failed to open the dedicated checkpoint connection; \
                             this tick is skipped and the open retried next tick"
                        );
                    } else {
                        tracing::debug!(
                            error = %e,
                            consecutive_failures = self.consecutive_open_failures,
                            "dedicated checkpoint connection still unavailable; \
                             this tick is skipped and the open retried next tick"
                        );
                    }
                    self.consecutive_open_failures =
                        self.consecutive_open_failures.saturating_add(1);
                    return None;
                }
            }
        }
        self.conn.as_ref()
    }

    /// Drop the current connection after a connection-level pragma failure so
    /// the next tick's `ensure_open` reopens it fresh rather than repeatedly
    /// retrying a connection already known to be broken.
    fn drop_connection(&mut self) {
        self.conn = None;
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
/// Issues `PRAGMA wal_checkpoint(PASSIVE)` every tick on the task's dedicated
/// `CheckpointConnection` — never the pool's writer mutex, so a concurrent
/// `pool.writer()` checkout can never queue behind a checkpoint tick. That
/// guarantee is admission-only: an armed TRUNCATE still takes SQLite's writer
/// lock and can block new write transactions, on any connection, for up to
/// `truncate_busy_timeout` (see `CheckpointConnection`'s contract). A tick is
/// `Skipped` only when that dedicated connection is itself unavailable. A
/// WARNING fires once per below→above threshold crossing, not every tick.
///
/// `lifecycle_owner` (ADR-094): exactly one task in a multi-backend fan-out
/// should receive `Some`. That task appends a best-effort
/// `CheckpointOutcomeRecorded` event on the elevation transition and one
/// recovery summary when pressure falls back below `warn_pages`. Sustained
/// elevated ticks aggregate in memory and in `db_diagnostics`; they never
/// write one primary-store row per checkpoint attempt. `None` explicitly
/// marks a non-owner. See `crates/khive-db/docs/api/checkpoint.md` for the
/// full shutdown-mechanism and event-emission design history.
///
/// `is_main` (ADR-091 Amendment 3): whether `pool` is the deployment's main
/// backend. A daemon owning several file-backed backends spawns one task per
/// backend, each with its own pool and shutdown-channel clone (the sender
/// broadcasts to every receiver clone alike). Lifecycle ownership is selected
/// independently through `lifecycle_owner`; `is_main` only controls registry
/// filtering. See the `tx_filter` construction below.
pub async fn run_checkpoint_task(
    pool: Arc<ConnectionPool>,
    config: CheckpointConfig,
    lifecycle_owner: Option<CheckpointLifecycleOwner>,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
    is_main: bool,
) {
    // This task IS the dedicated checkpoint owner: claim the pool so writer
    // connections drop the bounded autocheckpoint fallback and routine
    // checkpoint I/O stays off application commit paths. Pools without a
    // running checkpoint task never claim and keep SQLite's bounded WAL
    // reclamation. A failed claim leaves connections on the bounded fallback
    // — safe, just not the low-latency posture — so it warns and continues.
    match pool.claim_checkpoint_ownership() {
        Ok(()) => {
            if let Err(e) = pool.propagate_checkpoint_claim_to_writer_task().await {
                tracing::warn!(
                    error = %e,
                    "checkpoint task could not reach the writer task's connection; it keeps the \
                     bounded autocheckpoint fallback"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "checkpoint task could not re-apply the ownership pragma on the pooled writer; \
                 writer connections keep the bounded autocheckpoint fallback unless ownership is \
                 claimed later"
            );
        }
    }
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut severity_state = CheckpointSeverityState::default();
    let mut tx_age_state = TxAgeSweepState::default();
    let mut was_above_high_water = false;
    #[cfg(unix)]
    let legacy_walpin_fallback_interval = DEFAULT_SESSION_SWEEP_INTERVAL;
    #[cfg(unix)]
    let mut truncate_state =
        TruncateState::with_legacy_walpin_fallback(legacy_walpin_fallback_interval);
    #[cfg(not(unix))]
    let mut truncate_state = TruncateState::default();
    let mut lifecycle_emitter = CheckpointLifecycleEmitter::new(lifecycle_owner);
    // Independent of `severity_state` (which owns the WARN ladder): this
    // tracks the lifecycle sink's accepted elevation state. A full queue
    // leaves it unchanged, so an opening or recovery transition is retried
    // without admitting more than one primary-store write for that edge.
    let mut event_elevation_open = false;
    let mut pressure_episode: Option<CheckpointPressureEpisode> = None;
    // A recovery row whose `try_emit` lost the race against a full queue.
    // Retried on later ticks (before that tick's own transition handling)
    // instead of leaving `pressure_episode` open for a stale episode to
    // absorb the next, genuinely separate, pressure incident (#1857).
    let mut pending_recovery: Option<khive_storage::CheckpointOutcomeRecordedPayload> = None;
    let mut was_observed_above_warn = false;
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

    // Opened once here, at task startup; `ensure_open` is a no-op in steady
    // state and only reopens after a connection-level failure or (for an
    // in-memory/read-only pool) retries the open on every subsequent tick.
    let mut checkpoint_conn = CheckpointConnection::new();
    checkpoint_conn.ensure_open(&pool);

    loop {
        // A closed sender (the daemon returning without an explicit send)
        // makes `changed()` resolve with `Err` immediately, which `select!`
        // treats as ready — so shutdown is observed either way, not just on
        // an explicit send.
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.changed() => break,
        }

        #[cfg(unix)]
        truncate_state.begin_tick();

        #[cfg(unix)]
        let mut pending_sidecar_attribution = None;

        let tick = match checkpoint_conn.ensure_open(&pool) {
            None => {
                note_checkpoint_skipped();
                CheckpointTick::Skipped
            }
            Some(conn) => match checkpoint_once_core(&pool, conn, &config, &mut truncate_state) {
                Ok(outcome) => {
                    #[cfg(unix)]
                    {
                        pending_sidecar_attribution = outcome.sidecar_attribution;
                    }
                    #[cfg(not(unix))]
                    let _ = outcome.sidecar_attribution;
                    CheckpointTick::Observed(outcome.wal_pages)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "dedicated checkpoint connection failed a pragma; \
                         dropping it for a fresh reopen next tick"
                    );
                    checkpoint_conn.drop_connection();
                    note_checkpoint_skipped();
                    CheckpointTick::Skipped
                }
            },
        };

        // A successful no-progress TRUNCATE returns a bounded attribution
        // request alongside the core outcome. Consume it before any
        // report-derived decision or ordinary housekeeping for this tick.
        // The await is intentional: enumeration may perform up to 512
        // filesystem reads/classifications, so none of that work is allowed
        // to run on this Tokio worker, while one-pass-per-tick ordering still
        // requires the result (or an honest worker/enumeration failure) before
        // the fallback housekeeping arm is considered.
        #[cfg(unix)]
        if let Err(error) =
            complete_walpin_attribution(pending_sidecar_attribution, &mut truncate_state).await
        {
            tracing::warn!(
                error = %error,
                failure_kind = error.kind(),
                "ADR-091 Amendment 2 Plank B: no-progress sidecar attribution failed"
            );
        }

        // ADR-091 Plank 1: age-based sweep over the registry's oldest entry
        // MUST run on every tick, including a Skipped one — deliberately
        // BEFORE the Skipped early-continue below. Since the dedicated
        // checkpoint connection amendment, a `Skipped` tick means that
        // connection was itself unavailable, not that some registered span
        // was holding the pool's writer mutex (a checkpoint tick no longer
        // touches it at all) — but the sweep still must not go blind for the
        // duration of that outage, and the two failure surfaces are
        // independent: a registry span can go stale
        // (KHIVE_TX_WARN_SECS / KHIVE_TX_MAX_AGE_SECS) while wal_pages sits
        // well under warn_pages, or while the checkpoint connection itself is
        // down. Edge-triggered per rung, same debounce idiom as the severity
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
            let _ = run_walpin_housekeeping_if_due(
                sidecar,
                &mut truncate_state,
                legacy_walpin_fallback_interval,
            )
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
        note_checkpoint_pressure_observation(above_warn, was_observed_above_warn);
        was_observed_above_warn = above_warn;

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

        // ADR-094/#1838, #1857: one elevation row and one recovery summary
        // per genuinely continuous episode. Sustained elevated ticks update
        // only the bounded in-memory aggregate and process diagnostics
        // above; a dropped recovery handoff must not fold the next,
        // separate, pressure incident into this episode's aggregate.
        observe_checkpoint_pressure_tick(
            above_warn,
            wal_pages,
            above_high_water,
            above_truncate_high_water,
            &config,
            &mut event_elevation_open,
            &mut pressure_episode,
            &mut pending_recovery,
            |payload| {
                lifecycle_emitter
                    .try_emit(khive_types::EventKind::CheckpointOutcomeRecorded, payload)
            },
        );
    }

    lifecycle_emitter.shutdown().await;

    #[cfg(unix)]
    if let Some(sidecar) = walpin_state.as_mut() {
        sidecar.shutdown().await;
    }
}

/// Whether a `CheckpointOutcomeRecorded` transition should be enqueued for
/// this tick. Repeated observations in either state aggregate in memory;
/// only elevation and recovery edges reach the primary store.
fn checkpoint_outcome_should_emit(above_warn: bool, was_elevated: bool) -> bool {
    above_warn != was_elevated
}

/// Advance the pressure-episode/lifecycle-emission state machine for one
/// observed tick. `try_emit` mirrors [`CheckpointLifecycleEmitter::try_emit`]
/// — `true` means the row was handed off, `false` means the queue was full
/// or closed.
///
/// #1857: on a dropped recovery handoff (`try_emit` returns `false` while
/// `above_warn` is `false`), the closed episode's summary is stashed in
/// `pending_recovery` for retry on later ticks — flushed here before this
/// tick's own transition is evaluated — instead of leaving
/// `event_elevation_open` and `pressure_episode` open for the next elevated
/// tick to silently extend, which would report two separate pressure
/// incidents as one merged episode.
#[allow(clippy::too_many_arguments)]
fn observe_checkpoint_pressure_tick(
    above_warn: bool,
    wal_pages: u64,
    above_high_water: bool,
    above_truncate_high_water: bool,
    config: &CheckpointConfig,
    event_elevation_open: &mut bool,
    pressure_episode: &mut Option<CheckpointPressureEpisode>,
    pending_recovery: &mut Option<khive_storage::CheckpointOutcomeRecordedPayload>,
    mut try_emit: impl FnMut(khive_storage::CheckpointOutcomeRecordedPayload) -> bool,
) {
    // An undelivered recovery summary is a BARRIER, not merely a retry:
    // lifecycle consumers assert on the ordered event history (ADR-094), so
    // a later episode's opening must never be appended ahead of an earlier
    // episode's recovery. If the retry fails, the in-memory aggregate still
    // advances below, but no other emission is attempted this tick — a
    // deferred opening or recovery re-derives from state on a later tick,
    // after the pending summary has been delivered in order.
    let pending_blocks_emission = if let Some(payload) = pending_recovery.clone() {
        if try_emit(payload) {
            *pending_recovery = None;
            false
        } else {
            true
        }
    } else {
        false
    };

    if above_warn {
        match pressure_episode.as_mut() {
            Some(episode) => episode.observe(wal_pages),
            None => *pressure_episode = Some(CheckpointPressureEpisode::start(wal_pages)),
        }
    } else if !*event_elevation_open {
        // No elevation row reached the bounded handoff, so from any
        // consumer's view this episode never opened; discarding it keeps
        // the delivered history self-consistent. When the discard happens
        // because the barrier suppressed the opening attempt entirely, the
        // loss would otherwise be invisible even to the drop counters that
        // record failed attempts, so it is counted and logged here.
        if pending_blocks_emission && pressure_episode.is_some() {
            CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                wal_pages,
                "checkpoint pressure episode elapsed unreported behind an undelivered recovery summary"
            );
        }
        *pressure_episode = None;
    }

    if pending_blocks_emission || !checkpoint_outcome_should_emit(above_warn, *event_elevation_open)
    {
        return;
    }
    let Some(episode) = *pressure_episode else {
        tracing::warn!(
            above_warn,
            event_elevation_open = *event_elevation_open,
            "checkpoint pressure transition has no episode aggregate"
        );
        return;
    };
    let payload = khive_storage::CheckpointOutcomeRecordedPayload {
        wal_pages,
        warn_pages: config.warn_pages,
        high_water_pages: config.high_water_pages,
        truncate_high_water_pages: config.truncate_high_water_pages,
        above_warn,
        above_high_water,
        above_truncate_high_water,
        episode_elevated_ticks: Some(episode.elevated_ticks),
        episode_peak_wal_pages: Some(episode.peak_wal_pages),
    };
    if try_emit(payload.clone()) {
        *event_elevation_open = above_warn;
        if !above_warn {
            *pressure_episode = None;
        }
    } else if !above_warn {
        // The recovery handoff was dropped. Close this episode locally
        // anyway — `event_elevation_open` MUST NOT stay true, or the next
        // elevated tick would extend this (already finished) episode's
        // aggregate instead of starting a fresh one for what is genuinely a
        // new pressure incident. The dropped summary itself isn't thrown
        // away: it is stashed in `pending_recovery` and delivered on a
        // later tick, ahead of (and as a barrier to) every subsequent
        // emission, so lifecycle ordering survives the retry. The slot is
        // structurally empty here: a tick that entered with an undelivered
        // summary returned at the barrier above and never reached this arm.
        debug_assert!(
            pending_recovery.is_none(),
            "recovery emission attempted while an earlier summary was still pending"
        );
        *event_elevation_open = false;
        *pressure_episode = None;
        *pending_recovery = Some(payload);
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

/// Internal result of the synchronous SQLite checkpoint core. Keeping the
/// no-progress attribution request next to (but distinct from) `wal_pages`
/// makes the async handoff explicit and gives deferred work one caller-owned
/// lifetime instead of leaving it in mutable cross-tick state.
#[derive(Debug)]
#[must_use]
struct CheckpointCoreOutcome {
    wal_pages: u64,
    sidecar_attribution: Option<WalpinAttributionRequest>,
}

/// Issue one checkpoint cycle against the task's dedicated checkpoint
/// connection (`conn` — see `CheckpointConnection`; NEVER the pool's writer
/// mutex).
///
/// Returns the observed WAL page count on success. Returns `Err` only for a
/// connection-level pragma failure (the PASSIVE pragma itself erroring) — the
/// caller (`run_checkpoint_task`) treats that as a signal to drop `conn` and
/// lazily reopen a fresh one next tick, reporting the tick `Skipped`. Every
/// other error (e.g. a TRUNCATE attempt failing) is logged at warn level and
/// treated as non-fatal; the next tick retries against the same connection.
///
/// The caller owns all threshold-crossing WARN logging so that warnings fire
/// at most once per crossing, not every tick.
///
/// ADR-091 Plank 2: after the PASSIVE pass, this is also the single point
/// that may escalate to TRUNCATE (`maybe_truncate`) — on the SAME dedicated
/// connection, never a second connection or a pool checkout. A no-progress
/// result produces a separate cross-process attribution request; the
/// synchronous core never walks the sidecar directory. Production's
/// [`run_checkpoint_task`] consumes that request through an awaited
/// `spawn_blocking` before continuing the tick. This compatibility wrapper
/// intentionally returns only the historical page-count surface; the daemon
/// calls `checkpoint_once_core` so it cannot discard the request.
pub fn checkpoint_once(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
    config: &CheckpointConfig,
    truncate_state: &mut TruncateState,
) -> Result<u64, rusqlite::Error> {
    checkpoint_once_core(pool, conn, config, truncate_state).map(|outcome| outcome.wal_pages)
}

/// Synchronous PASSIVE/TRUNCATE core used by the async task. Unlike the
/// compatibility wrapper [`checkpoint_once`], this preserves the explicit
/// no-progress attribution request for the caller to complete off-runtime.
fn checkpoint_once_core(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
    config: &CheckpointConfig,
    truncate_state: &mut TruncateState,
) -> Result<CheckpointCoreOutcome, rusqlite::Error> {
    #[cfg(unix)]
    truncate_state.begin_tick();
    let raw_observation = match query_checkpoint_observation(conn) {
        Ok(observation) => observation,
        Err(e) => {
            tracing::warn!(error = %e, "WAL checkpoint failed");
            return Err(e);
        }
    };
    let observation = record_routine_wal_observation(pool, raw_observation);
    let wal_pages = observation.log_frames;
    LAST_WAL_PAGES.store(wal_pages, Ordering::Relaxed);
    note_checkpoint_observed(wal_pages);

    if raw_observation.busy != 0 {
        tracing::debug!(
            busy = raw_observation.busy,
            wal_log_frames = raw_observation.log_frames,
            wal_checkpointed_frames = raw_observation.checkpointed_frames,
            wal_pending_frames = observation.pending_frames,
            wal_physical_bytes = ?observation.physical_wal_bytes,
            "WAL PASSIVE checkpoint reported incomplete progress"
        );
    }
    tracing::debug!(
        wal_pages,
        wal_checkpointed_frames = observation.checkpointed_frames,
        wal_pending_frames = observation.pending_frames,
        wal_physical_bytes = ?observation.physical_wal_bytes,
        "WAL checkpoint issued"
    );

    let sidecar_attribution = maybe_truncate(pool, conn, config, wal_pages, truncate_state);

    Ok(CheckpointCoreOutcome {
        wal_pages,
        sidecar_attribution,
    })
}

/// Evaluate and, if due, attempt a TRUNCATE escalation on the same dedicated
/// checkpoint connection the caller already holds (never its own checkout —
/// there is no pool writer involved on this path at all). `last_attempt`
/// is stamped ONLY on an actual attempt, never on a skip. See
/// crates/khive-db/docs/api/checkpoint.md#maybe_truncate--truncate-attempt-gating-plank-2
fn maybe_truncate(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
    config: &CheckpointConfig,
    wal_pages_before: u64,
    truncate_state: &mut TruncateState,
) -> Option<WalpinAttributionRequest> {
    if wal_pages_before < config.truncate_high_water_pages {
        return None;
    }

    if let Some(last) = truncate_state.last_attempt {
        if last.elapsed() < config.truncate_min_interval {
            return None;
        }
    }

    // Which caller (if any) is pinning the WAL — logged before the attempt so
    // it is available even if the attempt itself succeeds.
    log_tx_registry_snapshot_warn(wal_pages_before);

    let original_busy_timeout = pool.config().busy_timeout;

    if let Err(e) = conn.busy_timeout(config.truncate_busy_timeout) {
        // Setup failed before the TRUNCATE pragma ever ran — this is a skip,
        // not an attempt. `last_attempt` must NOT advance here (ADR-091
        // §377-382): stamping now would suppress the next eligible attempt
        // for the full `truncate_min_interval` on a path that never touched
        // the WAL at all.
        tracing::warn!(error = %e, "failed to lower busy_timeout for TRUNCATE attempt; skipping");
        return None;
    }

    #[cfg(unix)]
    let mut holder_attribution = capture_walpin_attribution_request(pool, truncate_state);
    #[cfg(unix)]
    let mut sidecar_attribution = None;
    #[cfg(not(unix))]
    let sidecar_attribution = None;

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
                #[cfg(test)]
                if let Some(path) = pool.canonical_path() {
                    truncate_report_test_sync::after_no_progress_before_report(path);
                }
                #[cfg(unix)]
                {
                    // The census above had to be captured before TRUNCATE so
                    // a transient holder remains attributable. The bounded
                    // sidecar walk itself must not run here: this synchronous
                    // core is called directly from `run_checkpoint_task` on a
                    // Tokio worker. Hand the immutable request back to that
                    // async owner for an awaited `spawn_blocking` pass.
                    sidecar_attribution = holder_attribution.take();
                }
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
    #[cfg(unix)]
    if let Some(WalpinAttributionRequest::Fresh {
        previous_last_attempt,
        ..
    }) = holder_attribution.as_ref()
    {
        truncate_state.restore_walpin_full_scan_reservation(*previous_last_attempt);
    }
    sidecar_attribution
}

#[cfg(test)]
mod truncate_report_test_sync {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
    use std::sync::Mutex;

    struct Hook {
        db_path: PathBuf,
        reached_tx: SyncSender<()>,
        proceed_rx: Receiver<()>,
    }

    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

    pub(crate) fn install(db_path: PathBuf) -> (Receiver<()>, SyncSender<()>) {
        let (reached_tx, reached_rx) = sync_channel(0);
        let (proceed_tx, proceed_rx) = sync_channel(0);
        let replaced = HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Hook {
                db_path,
                reached_tx,
                proceed_rx,
            });
        assert!(replaced.is_none(), "truncate report hook already installed");
        (reached_rx, proceed_tx)
    }

    pub(crate) fn uninstall() {
        *HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(crate) fn after_no_progress_before_report(db_path: &Path) {
        let hook = {
            let mut guard = HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_ref() {
                Some(hook) if hook.db_path == db_path => guard.take(),
                _ => None,
            }
        };
        let Some(hook) = hook else {
            return;
        };
        let _ = hook.reached_tx.send(());
        let _ = hook.proceed_rx.recv();
    }
}

/// Deterministic seam for the async-attribution regressions below. The hook
/// executes inside the actual `spawn_blocking` closure, so a current-thread
/// Tokio test can prove both thread displacement and awaited ordering without
/// relying on sleeps or scheduler timing.
#[cfg(all(test, unix))]
mod walpin_attribution_test_sync {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
    use std::sync::{Arc, Mutex};

    enum Behavior {
        Pause {
            reached_tx: tokio::sync::oneshot::Sender<std::thread::ThreadId>,
            proceed_rx: Receiver<()>,
        },
        Panic,
    }

    struct Hook {
        dir: PathBuf,
        behavior: Behavior,
    }

    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    static REPORT_COUNTER: Mutex<Option<Arc<AtomicUsize>>> = Mutex::new(None);

    pub(crate) fn install_pause(
        dir: PathBuf,
    ) -> (
        tokio::sync::oneshot::Receiver<std::thread::ThreadId>,
        SyncSender<()>,
        Arc<AtomicUsize>,
    ) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = sync_channel(0);
        let report_counter = Arc::new(AtomicUsize::new(0));
        let replaced = HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Hook {
                dir,
                behavior: Behavior::Pause {
                    reached_tx,
                    proceed_rx,
                },
            });
        assert!(
            replaced.is_none(),
            "walpin attribution hook already installed"
        );
        *REPORT_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&report_counter));
        (reached_rx, proceed_tx, report_counter)
    }

    pub(crate) fn install_panic(dir: PathBuf) {
        let replaced = HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Hook {
                dir,
                behavior: Behavior::Panic,
            });
        assert!(
            replaced.is_none(),
            "walpin attribution hook already installed"
        );
    }

    pub(crate) fn uninstall() {
        *HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *REPORT_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(crate) fn before_enumeration(dir: &Path) {
        let hook = {
            let mut guard = HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_ref() {
                Some(hook) if hook.dir == dir => guard.take(),
                _ => None,
            }
        };
        let Some(hook) = hook else {
            return;
        };
        match hook.behavior {
            Behavior::Pause {
                reached_tx,
                proceed_rx,
            } => {
                if reached_tx.send(std::thread::current().id()).is_ok() {
                    let _ = proceed_rx.recv();
                }
            }
            Behavior::Panic => panic!("injected walpin attribution worker panic"),
        }
    }

    pub(crate) fn report_used() {
        if let Some(counter) = REPORT_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            counter.fetch_add(1, Ordering::SeqCst);
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

/// Immutable work captured around an armed TRUNCATE and consumed by the
/// async checkpoint owner only when that attempt makes no progress.
///
/// The holder census belongs here because it must precede the bounded
/// TRUNCATE wait. The sidecar directory walk does not: it remains deferred
/// until after the outcome is known and is executed through an awaited
/// `spawn_blocking` by [`complete_walpin_attribution`].
#[cfg(unix)]
#[derive(Debug)]
enum WalpinAttributionRequest {
    Fresh {
        dir: PathBuf,
        census: Result<crate::walpin::CensusResult, String>,
        legacy_fallback_interval: Duration,
        previous_last_attempt: Option<Instant>,
    },
    Cached(CachedWalpinAttribution),
    Suppressed,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalpinReportFreshness {
    Fresh,
    Cached { age: Duration },
}

#[cfg(unix)]
impl WalpinReportFreshness {
    fn is_fresh(self) -> bool {
        self == Self::Fresh
    }
}

/// Non-Unix placeholder keeps the synchronous core's outcome shape stable;
/// daemon sidecar attribution itself is Unix-only.
#[cfg(not(unix))]
type WalpinAttributionRequest = ();

/// Honest failure surface for an attempted no-progress attribution pass.
/// Both variants suppress same-tick housekeeping because a panicked blocking
/// worker may already have partially enumerated the directory; retrying a
/// second pass would violate the one-pass-per-tick bound.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum WalpinAttributionFailure {
    Enumeration(String),
    Worker(String),
}

#[cfg(unix)]
impl WalpinAttributionFailure {
    fn kind(&self) -> &'static str {
        match self {
            Self::Enumeration(_) => "enumeration",
            Self::Worker(_) => "blocking_worker",
        }
    }
}

#[cfg(unix)]
impl std::fmt::Display for WalpinAttributionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enumeration(error) => write!(
                formatter,
                "sidecar directory failed the trust-boundary enumeration; cross-process \
                 WAL-pin attribution is unestablished for this tick: {error}"
            ),
            Self::Worker(error) => write!(
                formatter,
                "sidecar attribution blocking worker failed; cross-process WAL-pin \
                 attribution is unestablished for this tick: {error}"
            ),
        }
    }
}

/// Capture the pre-TRUNCATE OS holder census and stable sidecar inputs. A
/// no-op if the sidecar is disabled or this backend has no on-disk path.
#[cfg(unix)]
fn capture_walpin_attribution_request(
    pool: &ConnectionPool,
    state: &mut TruncateState,
) -> Option<WalpinAttributionRequest> {
    let path = pool.canonical_path()?;
    if !crate::walpin::sidecar_enabled(true) {
        return None;
    }
    let legacy_fallback_interval = state.legacy_walpin_fallback_interval;
    Some(match state.plan_walpin_attribution_at(Instant::now()) {
        WalpinFullScanPlan::Refresh {
            previous_last_attempt,
        } => WalpinAttributionRequest::Fresh {
            dir: crate::walpin::sidecar_dir_for(path),
            census: crate::walpin::census_holders(path).map_err(|error| error.to_string()),
            legacy_fallback_interval,
            previous_last_attempt,
        },
        WalpinFullScanPlan::Cached(cached) => WalpinAttributionRequest::Cached(cached),
        WalpinFullScanPlan::Suppressed => WalpinAttributionRequest::Suppressed,
    })
}

/// Consume this tick's no-progress attribution request off the async runtime
/// worker and await it before any report or fallback housekeeping is used.
/// Returns `Ok(false)` when no pass was requested. Once a request exists the
/// state is marked attempted before spawning, so worker panic/cancellation
/// cannot accidentally authorize a second directory scan in the same tick.
#[cfg(unix)]
async fn complete_walpin_attribution(
    request: Option<WalpinAttributionRequest>,
    state: &mut TruncateState,
) -> Result<bool, WalpinAttributionFailure> {
    let Some(request) = request else {
        return Ok(false);
    };
    match request {
        WalpinAttributionRequest::Suppressed => Ok(false),
        WalpinAttributionRequest::Cached(cached) => {
            log_walpin_sidecar_report(
                &cached.report,
                cached.census,
                WalpinReportFreshness::Cached {
                    age: Instant::now().saturating_duration_since(cached.captured_at),
                },
            );
            Ok(true)
        }
        WalpinAttributionRequest::Fresh {
            dir,
            census,
            legacy_fallback_interval,
            previous_last_attempt: _,
        } => {
            state.sidecar_attribution_attempted_this_tick = true;
            if state.walpin_full_scan_last_attempt.is_none() {
                state.walpin_full_scan_last_attempt = Some(Instant::now());
            }
            let fallback = state.walpin_cached_attribution.clone();
            let result = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                walpin_attribution_test_sync::before_enumeration(&dir);
                crate::walpin::enumerate_live(&dir, legacy_fallback_interval)
            })
            .await
            .map_err(|error| WalpinAttributionFailure::Worker(error.to_string()))
            .and_then(|result| {
                result.map_err(|error| WalpinAttributionFailure::Enumeration(error.to_string()))
            });

            match result {
                Ok(report) => {
                    let captured_at = Instant::now();
                    log_walpin_sidecar_report(
                        &report,
                        census.clone(),
                        WalpinReportFreshness::Fresh,
                    );
                    state.cache_walpin_attribution(report, census, captured_at);
                    Ok(true)
                }
                Err(error) => {
                    if let Some(cached) = fallback {
                        log_walpin_sidecar_report(
                            &cached.report,
                            cached.census,
                            WalpinReportFreshness::Cached {
                                age: Instant::now().saturating_duration_since(cached.captured_at),
                            },
                        );
                    }
                    Err(error)
                }
            }
        }
    }
}

/// When a TRUNCATE attempt makes no progress, enumerate the walpin sidecar and
/// combine it with the holder census captured immediately before that attempt.
/// This pass consumes the classifications for attribution and returns whether
/// enumeration was attempted; the caller uses that marker to suppress the
/// ordinary housekeeping pass later in the same tick. Holder identity cannot
/// be deferred because a transient blocker may have released by then.
///
/// Sidecar-health attribution (ADR-091 Amendment 2):
/// the sharper "unregistered/native mechanism" conclusion is licensed only
/// when every discovered PID is `reporting` or `registered-silent`
/// (`WalpinReport::fully_attributed`); any `unknown` PID — including the
/// directory itself failing the trust-boundary check — makes attribution
/// inconclusive, and the WARN below names exactly which PIDs are unresolved
/// instead of silently exonerating them.
#[cfg(unix)]
fn log_walpin_sidecar_report(
    report: &crate::walpin::WalpinReport,
    census: Result<crate::walpin::CensusResult, String>,
    freshness: WalpinReportFreshness,
) {
    #[cfg(test)]
    walpin_attribution_test_sync::report_used();
    let now = now_epoch_secs();
    for hb in report.reporting() {
        // ADR-091 Amendment 3 Plank F2 fail-closed reading rule: the
        // logger must never let a fallback-confidence entry read as live
        // cross-process ground truth, so the confidence distinction is
        // always emitted alongside the raw field — never inferred by the
        // reader of this log line.
        tracing::warn!(
            walpin_pid = hb.pid,
            walpin_role = %hb.process_role,
            walpin_oldest_tx_age_secs = hb.current_oldest_tx_age_secs(now),
            walpin_oldest_tx_label = hb.oldest_tx_label.as_deref().unwrap_or("<unlabeled>"),
            walpin_attribution_basis = hb.attribution_basis.as_deref().unwrap_or("<unspecified>"),
            walpin_attribution_evidence_backed = hb.attribution_is_evidence_backed(),
            walpin_attribution_fresh = freshness.is_fresh(),
            walpin_health = "reporting",
            "ADR-091 Amendment 2 Plank B: live cross-process WAL-pin attribution report"
        );
    }
    for pid in report.registered_silent_pids() {
        tracing::debug!(
            walpin_pid = pid,
            walpin_health = "registered_silent",
            walpin_attribution_fresh = freshness.is_fresh(),
            "ADR-091 Amendment 2 Plank B: process affirmatively reports no over-threshold span"
        );
    }
    let mut unknown_pids: Vec<u32> = report.unknown_pids().collect();
    if let WalpinReportFreshness::Cached { age } = freshness {
        tracing::warn!(
            walpin_cache_age_ms = age.as_millis() as u64,
            "cached WAL-pin attribution is diagnostic-only; fully-attributed \
             conclusion is not licensed"
        );
        unknown_pids.push(0);
    }

    // The sidecar directory alone can only speak for PIDs that wrote
    // something there. Widen the universe to every PID the OS reports as
    // holding the database immediately before the TRUNCATE attempt; any holder
    // absent from `report` is unknown.
    match census {
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

    unknown_pids.sort_unstable();
    unknown_pids.dedup();
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

#[derive(Debug, Clone, Copy)]
struct RawCheckpointObservation {
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
}

/// Issue one PASSIVE checkpoint and retain the complete SQLite result row.
/// This is the periodic task's one routine checkpoint call: the same row
/// drives thresholds and the logical-backlog monitoring sample (#1849).
fn query_checkpoint_observation(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<RawCheckpointObservation> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok(RawCheckpointObservation {
            busy: row.get(0)?,
            log_frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    })
}

/// Query the current WAL frame count with one PASSIVE checkpoint.
///
/// Used only for rare post-TRUNCATE outcome measurement. The ordinary
/// periodic path calls [`query_checkpoint_observation`] directly and stores
/// its complete row, avoiding the former double-checkpoint pass.
fn query_wal_pages(conn: &rusqlite::Connection) -> u64 {
    let pages = query_checkpoint_observation(conn)
        .map(|observation| observation.log_frames)
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
    use crate::writer_task::WriterTaskHandle;
    use rusqlite::hooks::{AuthAction, Authorization};
    use serial_test::serial;
    use tracing::field::{Field, Visit};

    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        message: Option<String>,
        oldest_tx_label: Option<String>,
        tx_label: Option<String>,
        census_only: Option<String>,
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
                "census_only" => self.0.census_only = Some(cleaned),
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

    async fn writer_task_wal_autocheckpoint_pages(handle: &WriterTaskHandle) -> u32 {
        handle
            .send_top_level(|conn| {
                conn.pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, u32>(0))
                    .map_err(|error| khive_storage::error::StorageError::Pool {
                        operation: "test_wal_autocheckpoint".into(),
                        message: error.to_string(),
                    })
            })
            .await
            .expect("query writer-task connection pragma")
    }

    /// Test helper: open the same dedicated standalone connection
    /// `run_checkpoint_task` opens in production, for tests that drive
    /// `checkpoint_once` directly.
    fn checkpoint_conn(pool: &ConnectionPool) -> rusqlite::Connection {
        pool.open_standalone_writer()
            .expect("open dedicated checkpoint connection")
    }

    struct TruncateReportHookGuard;

    impl Drop for TruncateReportHookGuard {
        fn drop(&mut self) {
            truncate_report_test_sync::uninstall();
        }
    }

    #[cfg(unix)]
    struct WalpinAttributionHookGuard;

    #[cfg(unix)]
    impl Drop for WalpinAttributionHookGuard {
        fn drop(&mut self) {
            walpin_attribution_test_sync::uninstall();
        }
    }

    #[test]
    #[cfg(unix)]
    fn walpin_full_scan_cadence_refreshes_first_then_reuses_until_boundary() {
        let cadence = Duration::from_secs(30);
        let started_at = Instant::now();
        let mut state = TruncateState::with_walpin_full_scan_cadence(cadence);

        assert!(matches!(
            state.plan_walpin_attribution_at(started_at),
            WalpinFullScanPlan::Refresh { .. }
        ));
        state.cache_walpin_attribution(
            crate::walpin::WalpinReport::default(),
            Ok(crate::walpin::CensusResult::default()),
            started_at,
        );

        assert!(matches!(
            state.plan_walpin_attribution_at(started_at + cadence - Duration::from_nanos(1)),
            WalpinFullScanPlan::Cached(_)
        ));
        assert!(matches!(
            state.plan_walpin_attribution_at(started_at + cadence),
            WalpinFullScanPlan::Refresh { .. }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn walpin_full_scan_failure_retries_only_after_cadence() {
        let cadence = Duration::from_secs(30);
        let started_at = Instant::now();
        let mut state = TruncateState::with_walpin_full_scan_cadence(cadence);

        assert!(matches!(
            state.plan_walpin_attribution_at(started_at),
            WalpinFullScanPlan::Refresh { .. }
        ));
        // No cache update models either an enumeration error or a panicked
        // blocking worker. The attempt itself still owns the cadence slot.
        assert!(matches!(
            state.plan_walpin_attribution_at(started_at + cadence - Duration::from_nanos(1)),
            WalpinFullScanPlan::Suppressed
        ));
        assert!(matches!(
            state.plan_walpin_attribution_at(started_at + cadence),
            WalpinFullScanPlan::Refresh { .. }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn cached_walpin_report_is_diagnostic_only_even_when_fully_attributed() {
        let report = crate::walpin::WalpinReport::default();
        assert!(
            report.fully_attributed(),
            "the fixture must otherwise license the sharp conclusion"
        );
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };

        tracing::subscriber::with_default(subscriber, || {
            log_walpin_sidecar_report(
                &report,
                Ok(crate::walpin::CensusResult::default()),
                WalpinReportFreshness::Cached {
                    age: Duration::from_secs(1),
                },
            );
        });

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|event| {
                event.message.as_deref()
                    == Some(
                        "cached WAL-pin attribution is diagnostic-only; fully-attributed \
                         conclusion is not licensed",
                    )
            }),
            "cached attribution must declare its fail-closed status: {events:?}"
        );
        assert!(
            !events.iter().any(|event| {
                event.message.as_deref().is_some_and(|message| {
                    message.starts_with("ADR-091 Amendment 2 Plank B: every live PID is reporting")
                })
            }),
            "cached attribution must never authorize the fully-attributed conclusion: {events:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(unix)]
    #[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
    async fn progressing_truncate_releases_full_scan_reservation_to_housekeeping() {
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walpin-progress-reservation.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let conn = checkpoint_conn(&pool);
        let mut state = TruncateState::default();
        let config = CheckpointConfig {
            truncate_high_water_pages: 0,
            truncate_min_interval: Duration::ZERO,
            ..CheckpointConfig::default()
        };

        assert!(
            maybe_truncate(&pool, &conn, &config, u64::MAX, &mut state).is_none(),
            "a progressing TRUNCATE must not schedule no-progress attribution"
        );
        assert!(
            state.housekeeping_due(),
            "unused pre-TRUNCATE reservation must be restored before housekeeping"
        );
        let sidecar =
            WalpinSidecarState::new(pool.canonical_path(), true, "daemon", config.interval)
                .expect("file-backed test sidecar");
        assert!(
            run_walpin_housekeeping_if_due(&sidecar, &mut state, DEFAULT_SESSION_SWEEP_INTERVAL,)
                .await,
            "the production housekeeping arm must consume one full scan"
        );
        assert!(state.walpin_cached_attribution.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(unix)]
    #[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
    async fn erroring_truncate_releases_full_scan_reservation_to_housekeeping() {
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walpin-error-reservation.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let conn = checkpoint_conn(&pool);
        conn.authorizer(Some(
            |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Pragma { pragma_name, .. }
                    if pragma_name.eq_ignore_ascii_case("wal_checkpoint") =>
                {
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            },
        ))
        .unwrap();
        let mut state = TruncateState::default();
        let config = CheckpointConfig {
            truncate_high_water_pages: 0,
            truncate_min_interval: Duration::ZERO,
            ..CheckpointConfig::default()
        };

        assert!(
            maybe_truncate(&pool, &conn, &config, u64::MAX, &mut state).is_none(),
            "an erroring TRUNCATE must not schedule no-progress attribution"
        );
        conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert!(
            state.housekeeping_due(),
            "failed TRUNCATE must restore its unused full-scan reservation"
        );
        let sidecar =
            WalpinSidecarState::new(pool.canonical_path(), true, "daemon", config.interval)
                .expect("file-backed test sidecar");
        assert!(
            run_walpin_housekeeping_if_due(&sidecar, &mut state, DEFAULT_SESSION_SWEEP_INTERVAL,)
                .await,
            "the production housekeeping arm must consume one full scan"
        );
        assert!(state.walpin_cached_attribution.is_some());
    }

    struct ReaderProcess {
        child: std::process::Child,
        _stdout: std::io::BufReader<std::process::ChildStdout>,
    }

    impl ReaderProcess {
        fn spawn(db_path: &std::path::Path) -> Self {
            use std::io::BufRead;
            use std::process::Stdio;

            let mut child = std::process::Command::new(
                std::env::current_exe().expect("resolve current test executable"),
            )
            .args([
                "--exact",
                "checkpoint::tests::walpin_transient_reader_process_helper",
                "--nocapture",
            ])
            .env("KHIVE_CHECKPOINT_READER_HELPER_PATH", db_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn transient WAL reader helper");

            let stdout = child.stdout.take().expect("capture helper stdout");
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let bytes = reader
                    .read_line(&mut line)
                    .expect("read transient reader readiness signal");
                assert!(bytes > 0, "reader helper exited before readiness signal");
                if line.contains("KHIVE_CHECKPOINT_READER_READY") {
                    break;
                }
            }
            Self {
                child,
                _stdout: reader,
            }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn release(&mut self) {
            use std::io::Write;

            let mut stdin = self.child.stdin.take().expect("helper stdin is available");
            stdin
                .write_all(b"release\n")
                .expect("release transient reader");
            drop(stdin);
            let status = self.child.wait().expect("wait for transient reader helper");
            assert!(status.success(), "transient reader helper failed: {status}");
        }
    }

    impl Drop for ReaderProcess {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[test]
    fn walpin_transient_reader_process_helper() {
        use std::io::Write;

        let Some(path) = std::env::var_os("KHIVE_CHECKPOINT_READER_HELPER_PATH") else {
            return;
        };
        let conn = rusqlite::Connection::open(path).expect("helper opens database");
        conn.execute_batch("BEGIN DEFERRED; SELECT * FROM t;")
            .expect("helper pins a read snapshot");
        println!("KHIVE_CHECKPOINT_READER_READY");
        std::io::stdout().flush().expect("flush readiness signal");
        let mut release = String::new();
        std::io::stdin()
            .read_line(&mut release)
            .expect("wait for release signal");
        conn.execute_batch("COMMIT")
            .expect("helper releases read snapshot");
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(unix)]
    #[serial(
        checkpoint_skip_metrics,
        khive_walpin_sidecar_env,
        walpin_attribution_async,
        walpin_report_seam
    )]
    async fn no_progress_report_keeps_holder_released_after_truncate_timeout() {
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("transient-reader.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .expect("seed WAL before reader snapshot");
        }

        let mut reader = ReaderProcess::spawn(&path);
        let reader_pid = reader.pid();
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch("INSERT INTO t VALUES (2);")
                .expect("append WAL behind reader snapshot");
        }

        let canonical_path = pool
            .canonical_path()
            .expect("file-backed pool has canonical path")
            .to_path_buf();
        let (reached_rx, proceed_tx) = truncate_report_test_sync::install(canonical_path.clone());
        let _hook_guard = TruncateReportHookGuard;
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let checkpoint_pool = Arc::clone(&pool);
        let dedicated_conn = checkpoint_conn(&checkpoint_pool);
        let checkpoint = std::thread::spawn(move || {
            let mut state = TruncateState::default();
            let result = checkpoint_once_core(
                &checkpoint_pool,
                &dedicated_conn,
                &CheckpointConfig {
                    truncate_high_water_pages: 0,
                    truncate_min_interval: Duration::ZERO,
                    truncate_busy_timeout: Duration::from_millis(50),
                    ..CheckpointConfig::default()
                },
                &mut state,
            );
            (result, state)
        });

        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("TRUNCATE must report no progress while the reader is pinned");
        reader.release();
        let post_attempt_census =
            crate::walpin::census_holders(&canonical_path).expect("post-attempt holder census");
        assert!(
            !post_attempt_census.holders.contains(&reader_pid),
            "released reader PID must be absent from a post-attempt census"
        );
        proceed_tx
            .send(())
            .expect("allow no-progress reporting to continue");
        let (checkpoint_result, mut state) = checkpoint.join().expect("checkpoint thread");
        let outcome = checkpoint_result.expect("checkpoint succeeds");
        assert!(
            outcome.sidecar_attribution.is_some(),
            "the synchronous checkpoint result must carry a separate attribution request"
        );
        assert!(
            !state.sidecar_attribution_attempted_this_tick,
            "capturing a request is not the same as attempting its directory enumeration"
        );

        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let attribution_attempted =
            complete_walpin_attribution(outcome.sidecar_attribution, &mut state)
                .await
                .expect("deferred attribution succeeds");
        assert!(
            attribution_attempted,
            "a no-progress attribution pass must suppress the redundant healthy-housekeeping \
             pass for the same tick"
        );

        let events = buffer.lock().expect("captured events");
        assert!(
            events.iter().any(|event| {
                event
                    .census_only
                    .as_deref()
                    .is_some_and(|pids| pids.contains(&reader_pid.to_string()))
            }),
            "the no-progress report must retain PID {reader_pid} from the pre-attempt census: {events:?}"
        );
    }

    /// The 512-entry attribution walk must run on Tokio's blocking pool and
    /// the async owner must await it before the report is consumed. A paused
    /// real enumeration proves all three facts without a timing sleep: its
    /// thread differs from the current-thread runtime, the completion future
    /// remains pending, and the report-use counter stays zero until release.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(unix)]
    #[serial(walpin_attribution_async)]
    async fn no_progress_attribution_is_off_runtime_and_awaited_before_report_use() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().expect("tempdir");
        let sidecar_dir = dir.path().join("checkpoint.db.walpin");
        let (reached_rx, proceed_tx, report_counter) =
            walpin_attribution_test_sync::install_pause(sidecar_dir.clone());
        let _hook_guard = WalpinAttributionHookGuard;

        let runtime_thread = std::thread::current().id();
        let mut state = TruncateState::default();
        let request = Some(WalpinAttributionRequest::Fresh {
            dir: sidecar_dir,
            census: Ok(crate::walpin::CensusResult::default()),
            legacy_fallback_interval: DEFAULT_SESSION_SWEEP_INTERVAL,
            previous_last_attempt: None,
        });
        let completion = tokio::spawn(async move {
            let result = complete_walpin_attribution(request, &mut state).await;
            (result, state)
        });

        let blocking_thread = reached_rx
            .await
            .expect("spawn_blocking attribution reached test seam");
        assert_ne!(
            blocking_thread, runtime_thread,
            "sidecar enumeration must not execute on the current-thread Tokio runtime worker"
        );
        assert!(
            !completion.is_finished(),
            "the async attribution owner must await the still-paused blocking enumeration"
        );
        assert_eq!(
            report_counter.load(Ordering::SeqCst),
            0,
            "the attribution report must not be consumed before enumeration completes"
        );

        proceed_tx
            .send(())
            .expect("release blocking attribution enumeration");
        let (result, state) = completion.await.expect("attribution task joins");
        assert_eq!(result, Ok(true));
        assert_eq!(
            report_counter.load(Ordering::SeqCst),
            1,
            "the completed enumeration must feed exactly one report use"
        );
        assert!(state.sidecar_attribution_attempted_this_tick);
        assert!(
            !state.housekeeping_due(),
            "completed attribution must suppress same-tick housekeeping"
        );
    }

    /// A panicked blocking worker is not flattened into success. The tick is
    /// still marked attempted because the worker may have partially walked
    /// the directory before failing, so starting housekeeping afterward
    /// would violate the one-pass bound.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(unix)]
    #[serial(walpin_attribution_async)]
    async fn no_progress_attribution_join_failure_is_honest_and_suppresses_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sidecar_dir = dir.path().join("checkpoint.db.walpin");
        walpin_attribution_test_sync::install_panic(sidecar_dir.clone());
        let _hook_guard = WalpinAttributionHookGuard;

        let mut state = TruncateState::default();
        let request = Some(WalpinAttributionRequest::Fresh {
            dir: sidecar_dir,
            census: Ok(crate::walpin::CensusResult::default()),
            legacy_fallback_interval: DEFAULT_SESSION_SWEEP_INTERVAL,
            previous_last_attempt: None,
        });

        let error = complete_walpin_attribution(request, &mut state)
            .await
            .expect_err("injected worker panic must surface as failure");
        assert!(
            matches!(error, WalpinAttributionFailure::Worker(_)),
            "join failure must retain its worker classification: {error:?}"
        );
        assert!(state.sidecar_attribution_attempted_this_tick);
        assert!(
            !state.housekeeping_due(),
            "an indeterminate partial pass must not authorize a second scan"
        );
    }

    /// Structural guard for the accepted ADR split: the synchronous SQLite
    /// checkpoint core only schedules attribution, the sole direct
    /// `enumerate_live` call is nested in an awaited `spawn_blocking`, and the
    /// task completes it before either housekeeping or lifecycle outcome use.
    #[test]
    #[cfg(unix)]
    #[serial(checkpoint_skip_metrics)]
    fn async_checkpoint_source_keeps_enumeration_behind_awaited_spawn_blocking() {
        fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
            source
                .split_once(start)
                .unwrap_or_else(|| panic!("missing source marker {start:?}"))
                .1
                .split_once(end)
                .unwrap_or_else(|| panic!("missing source marker {end:?}"))
                .0
        }

        let source = include_str!("checkpoint.rs");
        let checkpoint_core = section(
            source,
            "fn checkpoint_once_core(",
            "/// Evaluate and, if due, attempt a TRUNCATE escalation",
        );
        let truncate_core = section(
            source,
            "fn maybe_truncate(",
            "#[cfg(test)]\nmod truncate_report_test_sync",
        );
        let report_logger = section(
            source,
            "fn log_walpin_sidecar_report(",
            "/// ADR-091 Amendment 2 Plank C",
        );
        for (name, body) in [
            ("checkpoint_once_core", checkpoint_core),
            ("maybe_truncate", truncate_core),
            ("log_walpin_sidecar_report", report_logger),
        ] {
            assert!(
                !body.contains("enumerate_live("),
                "{name} must not perform direct sidecar enumeration"
            );
        }

        let async_completion = section(
            source,
            "async fn complete_walpin_attribution(",
            "/// When a TRUNCATE attempt makes no progress",
        );
        let spawn = async_completion
            .find("tokio::task::spawn_blocking")
            .expect("completion must spawn blocking work");
        let enumerate = async_completion
            .find("crate::walpin::enumerate_live")
            .expect("blocking closure must perform the attribution enumeration");
        let awaited = async_completion[enumerate..]
            .find(".await")
            .map(|offset| enumerate + offset)
            .expect("blocking worker must be awaited");
        assert!(spawn < enumerate && enumerate < awaited);

        let task = section(
            source,
            "pub async fn run_checkpoint_task(",
            "/// Whether a `CheckpointOutcomeRecorded` transition should be enqueued",
        );
        let checkpoint = task
            .find("checkpoint_once_core(")
            .expect("checkpoint core call");
        let completion = task
            .find("complete_walpin_attribution(")
            .expect("awaited attribution completion");
        let housekeeping = task
            .find("run_walpin_housekeeping_if_due(")
            .expect("fallback housekeeping");
        let outcome = task
            .find("observe_checkpoint_pressure_tick(")
            .expect("lifecycle outcome use");
        assert!(
            checkpoint < completion && completion < housekeeping && housekeeping < outcome,
            "tick ordering must be checkpoint -> awaited attribution -> housekeeping decision -> outcome"
        );
        let housekeeping_helper = section(
            source,
            "async fn run_walpin_housekeeping_if_due(",
            "fn now_epoch_secs()",
        );
        assert!(
            housekeeping_helper.contains("reap_dead_entries_bounded(legacy_fallback_interval)"),
            "the ordered housekeeping arm must retain the bounded full scan"
        );

        // The outcome decision itself moved into the extracted per-tick
        // helper; the emit gate must still be consulted there, so the
        // ordering assertion above remains transitively about the same
        // lifecycle decision it always pinned.
        let pressure_tick = section(
            source,
            "fn observe_checkpoint_pressure_tick(",
            "/// ADR-091 Plank 0",
        );
        assert!(
            pressure_tick.contains("checkpoint_outcome_should_emit"),
            "extracted pressure tick helper must gate on the lifecycle emit decision"
        );
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

        let conn = checkpoint_conn(&pool);
        checkpoint_once(
            &pool,
            &conn,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        )
        .expect("checkpoint_once must succeed against a healthy dedicated connection");
    }

    /// In-memory pools have no on-disk file to open a second, dedicated
    /// standalone connection against — this is exactly the precondition that
    /// makes `CheckpointConnection::ensure_open` return `None` and
    /// `run_checkpoint_task` report the tick `Skipped`, so `checkpoint_once`
    /// is never even called for one.
    #[test]
    fn open_standalone_writer_fails_on_in_memory_pool() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(cfg).expect("in-memory pool"));
        assert!(
            pool.open_standalone_writer().is_err(),
            "an in-memory pool must not be able to open a dedicated checkpoint connection"
        );
    }

    /// `CheckpointConnection::ensure_open` must open its dedicated connection
    /// through the untracked standalone boundary, both on the initial open
    /// and on a reopen after the connection is dropped — the checkpoint task
    /// is exempt infrastructure, not a request-traffic writer acquisition
    /// (ADR-136 D1 gate 5).
    #[test]
    fn ensure_open_does_not_move_writer_acquisition_counters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint_ensure_open.db");
        let pool = file_pool(&path);

        let before_first_open = pool.writer_acquisition_snapshot();
        let mut checkpoint_conn = CheckpointConnection::new();
        checkpoint_conn
            .ensure_open(&pool)
            .expect("dedicated checkpoint connection must open against a file-backed pool");
        assert_eq!(
            pool.writer_acquisition_snapshot(),
            before_first_open,
            "the checkpoint connection's initial open must not count as a writer acquisition"
        );

        checkpoint_conn.conn = None;
        let before_reopen = pool.writer_acquisition_snapshot();
        checkpoint_conn
            .ensure_open(&pool)
            .expect("dedicated checkpoint connection must reopen after invalidation");
        assert_eq!(
            pool.writer_acquisition_snapshot(),
            before_reopen,
            "reopening the checkpoint connection must not count as a writer acquisition either"
        );
    }

    #[test]
    fn checkpoint_connection_disables_wal_autocheckpoint_on_open_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint_autocheckpoint.db");
        let pool = file_pool(&path);
        let mut checkpoint_conn = CheckpointConnection::new();

        let initial: u32 = checkpoint_conn
            .ensure_open(&pool)
            .expect("dedicated checkpoint connection must open")
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .expect("read initial autocheckpoint setting");
        assert_eq!(initial, 0);

        checkpoint_conn.conn = None;
        let reopened: u32 = checkpoint_conn
            .ensure_open(&pool)
            .expect("dedicated checkpoint connection must reopen")
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .expect("read reopened autocheckpoint setting");
        assert_eq!(reopened, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial(checkpoint_skip_metrics)]
    async fn failed_checkpoint_claim_keeps_existing_writer_task_on_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed_claim_writer_task.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                checkout_timeout: Duration::from_millis(1),
                write_queue_enabled: Some(true),
                ..PoolConfig::default()
            })
            .expect("pool open"),
        );
        let writer_task = pool
            .writer_task_handle()
            .expect("writer-task resolution")
            .expect("writer task enabled");
        assert_eq!(
            writer_task_wal_autocheckpoint_pages(&writer_task).await,
            crate::pool::FALLBACK_WAL_AUTOCHECKPOINT_PAGES
        );

        let legacy_conn = pool.legacy_conn();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = tokio::task::spawn_blocking(move || {
            let _held_writer = legacy_conn.lock();
            held_tx.send(()).expect("signal held pooled writer");
            release_rx.recv().expect("release held pooled writer");
        });
        held_rx.await.expect("pooled writer holder started");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        drop(shutdown_tx);
        run_checkpoint_task(
            Arc::clone(&pool),
            CheckpointConfig {
                interval: Duration::from_secs(60),
                ..CheckpointConfig::default()
            },
            None,
            shutdown_rx,
            true,
        )
        .await;

        assert_eq!(pool.writer_acquisition_snapshot().timeouts, 1);
        assert_eq!(
            writer_task_wal_autocheckpoint_pages(&writer_task).await,
            crate::pool::FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            "failed pooled-writer claim must not partially propagate ownership"
        );
        release_tx.send(()).expect("release pooled writer");
        holder.await.expect("pooled writer holder joined");
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
        let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

        shutdown_tx.send(()).expect("send shutdown signal");

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(checkpoint_skip_metrics, khive_walpin_sidecar_env)]
    async fn healthy_checkpoint_tick_reaps_a_dead_walpin_beacon_without_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("healthy_sidecar_reap.db");
        let pool = file_pool(&path);
        let sidecar_dir =
            crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed pool"));
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");

        let dead_pid = 2_000_000_000;
        let dead_beacon = crate::walpin::WalpinBeacon {
            pid: dead_pid,
            process_role: "session".to_string(),
            started_at: 1,
            sweep_interval_ms: 5_000,
        };
        crate::walpin::write_beacon(&sidecar_dir, &dead_beacon)
            .expect("seed a crashed process's orphan beacon");
        let dead_beacon_path = crate::walpin::beacon_path(&sidecar_dir, dead_pid);
        assert!(dead_beacon_path.exists(), "orphan fixture must exist");

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: u64::MAX,
            high_water_pages: u64::MAX,
            truncate_high_water_pages: u64::MAX,
            ..CheckpointConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

        let reaped = wait_for(Duration::from_secs(2), || !dead_beacon_path.exists()).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        assert!(
            reaped,
            "the ordinary healthy tick must reap positively dead sidecar residue independently of \
             TRUNCATE diagnostics"
        );
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
            Some(CheckpointLifecycleOwner::new(event_store, "local")),
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

        let conn = checkpoint_conn(&pool);
        let start = std::time::Instant::now();
        checkpoint_once(
            &pool,
            &conn,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        )
        .expect("checkpoint_once must succeed against a healthy dedicated connection");
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

        let conn = checkpoint_conn(&pool);
        checkpoint_once(&pool, &conn, &config, &mut state)
            .expect("checkpoint_once must succeed against a healthy dedicated connection");
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

        let conn = checkpoint_conn(&pool);
        checkpoint_once(&pool, &conn, &config, &mut state)
            .expect("checkpoint_once must succeed against a healthy dedicated connection");

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
        let conn = checkpoint_conn(&pool);

        checkpoint_once(&pool, &conn, &config, &mut state)
            .expect("checkpoint_once must succeed against a healthy dedicated connection");
        let first_attempt = state.last_attempt.expect("first tick must attempt");

        // Second tick, immediately after, on the SAME dedicated connection
        // (mirroring how `run_checkpoint_task` reuses one connection across
        // ticks): still above threshold, but the min-interval has clearly
        // not elapsed — must skip and leave last_attempt exactly as it was.
        checkpoint_once(&pool, &conn, &config, &mut state)
            .expect("checkpoint_once must succeed against a healthy dedicated connection");
        let second_attempt = state.last_attempt.expect("attempt timestamp must persist");

        assert_eq!(
            first_attempt, second_attempt,
            "a tick within truncate_min_interval must not re-stamp last_attempt"
        );
    }

    /// The fix this module exists to prove: holding the POOL's writer mutex
    /// (via `pool.try_writer()`, exactly like a concurrent write in
    /// progress) must NOT cause `checkpoint_once` to skip — PASSIVE (and, if
    /// armed, TRUNCATE) run on the task's own dedicated connection, which
    /// never contends with the pool writer at all. Before the dedicated-
    /// connection fix, this same setup made `checkpoint_once` return
    /// `Skipped` via `try_writer_nowait()`. See also the standalone
    /// integration reproducer in `tests/checkpoint_dedicated_connection.rs`,
    /// which demonstrates the converse: a fat WAL held busy by
    /// `checkpoint_once` no longer blocks a concurrent `pool.writer()`
    /// admission either.
    #[test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    fn checkpoint_once_proceeds_and_can_attempt_truncate_while_pool_writer_held() {
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

        let conn = checkpoint_conn(&pool);

        // Hold the POOL's writer mutex for the duration of the checkpoint_once
        // call, acquired BEFORE the call so the dedicated connection cannot
        // possibly race a still-free writer.
        let _held = pool.try_writer().unwrap();

        let config = CheckpointConfig {
            truncate_high_water_pages: 0,
            ..CheckpointConfig::default()
        };
        let mut state = TruncateState::default();

        checkpoint_once(&pool, &conn, &config, &mut state).expect(
            "checkpoint_once must observe normally on its own dedicated connection even \
             while a concurrent caller holds the pool's writer mutex",
        );

        assert!(
            state.last_attempt.is_some(),
            "a threshold-armed tick must still evaluate (and attempt) TRUNCATE even while \
             the pool writer is held — the dedicated connection is unaffected by it"
        );
        assert_eq!(
            checkpoint_skipped_ticks(),
            0,
            "a busy pool writer must no longer count as a skipped checkpoint tick"
        );
        assert_eq!(
            checkpoint_consecutive_skips(),
            0,
            "a busy pool writer must not bump the consecutive-skip run length"
        );
    }

    /// Regression guard for #845 (a recurrence of the #828 shared-statics
    /// race): every test in this module that calls `checkpoint_once`,
    /// `checkpoint_once_core`, or `run_checkpoint_task` — all funnel through
    /// `query_wal_pages`, which
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
    #[serial(checkpoint_skip_metrics)]
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

            let touches_shared_metrics = span.iter().any(|l| {
                l.contains("checkpoint_once(")
                    || l.contains("checkpoint_once_core(")
                    || l.contains("run_checkpoint_task(")
            });
            if !touches_shared_metrics {
                continue;
            }

            // Rustfmt splits long multi-key attributes across lines, so scan
            // the whole attribute instead of requiring the group on `#[serial(`.
            let mut in_serial_attr = false;
            let has_group_tag = span.iter().any(|line| {
                let trimmed = line.trim();
                if !in_serial_attr {
                    in_serial_attr = trimmed.starts_with("#[serial(");
                }
                if !in_serial_attr {
                    return false;
                }

                let has_group = trimmed
                    .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .any(|token| token == "checkpoint_skip_metrics");
                if trimmed.ends_with(")]") {
                    in_serial_attr = false;
                }
                has_group
            });

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
            "these tests call checkpoint_once/checkpoint_once_core/run_checkpoint_task (which write the \
             process-wide LAST_WAL_PAGES/CHECKPOINT_* atomics via query_wal_pages) but \
             are not tagged #[serial(checkpoint_skip_metrics)] (or a group including it); \
             an untagged caller running concurrently on cargo's default test thread pool \
             can clobber those atomics mid-assertion in another test (the #828/#845 race): \
             {offenders:?}"
        );
    }

    /// Observation branch: a checkpoint tick that is actually observed
    /// (dedicated connection available) must close out a prior skip streak,
    /// resetting the consecutive-skip counter to 0 without touching the
    /// lifetime total. Drives `note_checkpoint_skipped()` directly for the
    /// skipped ticks — exactly what `run_checkpoint_task` calls when
    /// `CheckpointConnection::ensure_open` returns `None` — rather than
    /// through pool-writer contention, which (since the dedicated-connection
    /// fix) no longer produces a skipped tick at all.
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

        // Two consecutive skipped ticks (dedicated connection unavailable).
        note_checkpoint_skipped();
        note_checkpoint_skipped();
        assert_eq!(checkpoint_skipped_ticks(), 2);
        assert_eq!(checkpoint_consecutive_skips(), 2);

        // Now the dedicated connection is available: an observed tick must
        // reset the streak.
        let conn = checkpoint_conn(&pool);
        let mut state = TruncateState::default();
        checkpoint_once(&pool, &conn, &CheckpointConfig::default(), &mut state)
            .expect("checkpoint_once must succeed against a healthy dedicated connection");

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

        let conn = checkpoint_conn(&pool);
        let wal_pages = checkpoint_once(&pool, &conn, &config, &mut TruncateState::default())
            .expect("checkpoint_once must succeed against a healthy dedicated connection");
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

    #[derive(Clone, Copy)]
    enum FakeAppendBehavior {
        Record,
        Fail,
    }

    struct FakeEventStore {
        events: std::sync::Mutex<Vec<khive_storage::Event>>,
        append_attempts: std::sync::atomic::AtomicUsize,
        append_behavior: FakeAppendBehavior,
    }

    impl Default for FakeEventStore {
        fn default() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
                append_attempts: std::sync::atomic::AtomicUsize::new(0),
                append_behavior: FakeAppendBehavior::Record,
            }
        }
    }

    impl FakeEventStore {
        fn failing() -> Self {
            Self {
                append_behavior: FakeAppendBehavior::Fail,
                ..Self::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl khive_storage::EventStore for FakeEventStore {
        async fn append_event(
            &self,
            event: khive_storage::Event,
        ) -> khive_storage::StorageResult<()> {
            self.append_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match self.append_behavior {
                FakeAppendBehavior::Record => {
                    self.events.lock().unwrap().push(event);
                    Ok(())
                }
                FakeAppendBehavior::Fail => Err(khive_storage::StorageError::Internal(
                    "synthetic checkpoint lifecycle append failure".to_string(),
                )),
            }
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
    /// `checkpoint_outcome_should_emit` can see: elevation, sustained
    /// pressure, recovery, and repeated healthy observations.
    #[test]
    fn checkpoint_outcome_should_emit_covers_all_transitions() {
        assert!(
            checkpoint_outcome_should_emit(true, false),
            "first elevated tick must emit"
        );
        assert!(
            !checkpoint_outcome_should_emit(true, true),
            "sustained elevated ticks must aggregate in memory instead of writing the WAL"
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

    /// Regression #1838: under a persistent WAL pin, lifecycle persistence
    /// must scale with pressure-state transitions, not checkpoint attempts.
    #[test]
    fn persistent_pressure_lifecycle_rows_are_o_state_transitions() {
        let observations = [true; 128].into_iter().chain([false]).chain([false; 128]);
        let mut was_elevated = false;
        let writes = observations
            .filter(|above_warn| {
                let emit = checkpoint_outcome_should_emit(*above_warn, was_elevated);
                if emit {
                    was_elevated = *above_warn;
                }
                emit
            })
            .count();

        assert_eq!(
            writes, 2,
            "one elevation row plus one recovery summary must cover any number of attempts"
        );
    }

    #[test]
    fn checkpoint_pressure_episode_retains_recovery_summary() {
        let mut episode = CheckpointPressureEpisode::start(2_500);
        episode.observe(2_300);
        episode.observe(8_100);
        episode.observe(4_000);

        assert_eq!(episode.elevated_ticks, 4);
        assert_eq!(episode.peak_wal_pages, 8_100);
    }

    /// Drives [`observe_checkpoint_pressure_tick`] through a fixed sequence
    /// of `(above_warn, wal_pages)` ticks, faking `try_emit` per call index
    /// (0-based across the whole sequence) via `fail_on`. Returns every
    /// payload that was reported as successfully delivered, in delivery
    /// order.
    fn drive_pressure_ticks(
        config: &CheckpointConfig,
        ticks: &[(bool, u64)],
        mut fail_on: impl FnMut(usize) -> bool,
    ) -> Vec<khive_storage::CheckpointOutcomeRecordedPayload> {
        let mut event_elevation_open = false;
        let mut pressure_episode: Option<CheckpointPressureEpisode> = None;
        let mut pending_recovery: Option<khive_storage::CheckpointOutcomeRecordedPayload> = None;
        let mut delivered = Vec::new();
        let mut call_index = 0usize;
        for &(above_warn, wal_pages) in ticks {
            observe_checkpoint_pressure_tick(
                above_warn,
                wal_pages,
                false,
                false,
                config,
                &mut event_elevation_open,
                &mut pressure_episode,
                &mut pending_recovery,
                |payload| {
                    let idx = call_index;
                    call_index += 1;
                    if fail_on(idx) {
                        false
                    } else {
                        delivered.push(payload);
                        true
                    }
                },
            );
        }
        delivered
    }

    /// #1857 regression: a dropped recovery handoff must not fold the next,
    /// separate, pressure incident into the closed episode's aggregate — and
    /// the undelivered recovery is a BARRIER, so episode 2's opening must not
    /// be delivered ahead of episode 1's recovery (ADR-094: consumers assert
    /// on the ordered event history).
    ///
    /// Sequence: episode 1 opens and sustains for 3 ticks, then its recovery
    /// row is dropped (call index 1) and two retries are also dropped (call
    /// indices 2 and 3) — the second of them on the tick where episode 2
    /// begins, so the barrier defers episode 2's opening. Once the worker
    /// frees up, episode 1's delayed recovery delivers first, then episode
    /// 2's opening (reflecting its state at emission time), then episode 2's
    /// recovery.
    #[test]
    fn dropped_recovery_handoff_does_not_merge_pressure_episodes() {
        let config = CheckpointConfig {
            warn_pages: 1_000,
            ..CheckpointConfig::default()
        };
        let ticks = [
            (true, 1_500), // call 0: episode 1 opens — delivered
            (true, 1_800), // sustained, no emit attempt
            (true, 2_000), // sustained, no emit attempt
            (false, 500),  // call 1: episode 1 recovery — DROPPED
            (false, 400),  // call 2: retry episode 1 recovery — DROPPED
            (true, 3_000), // call 3: retry — DROPPED; barrier defers episode 2's opening
            (true, 3_500), // call 4: retry — delivered; call 5: episode 2 opens — delivered
            (false, 300),  // call 6: episode 2 recovery — delivered
        ];

        let delivered = drive_pressure_ticks(&config, &ticks, |idx| matches!(idx, 1..=3));

        assert_eq!(
            delivered.len(),
            4,
            "expected episode-1 open, episode-1 delayed recovery, episode-2 open, \
             episode-2 recovery: {delivered:?}"
        );

        let ep1_open = &delivered[0];
        assert!(ep1_open.above_warn);
        assert_eq!(ep1_open.episode_elevated_ticks, Some(1));
        assert_eq!(ep1_open.episode_peak_wal_pages, Some(1_500));

        let ep1_recovery = &delivered[1];
        assert!(
            !ep1_recovery.above_warn,
            "episode 1's recovery must be delivered BEFORE episode 2's opening; \
             an opening in this slot means the barrier failed: {delivered:?}"
        );
        assert_eq!(
            ep1_recovery.episode_elevated_ticks,
            Some(3),
            "episode 1's delayed recovery must report only its own 3 elevated ticks, \
             not ticks absorbed from episode 2"
        );
        assert_eq!(ep1_recovery.episode_peak_wal_pages, Some(2_000));

        let ep2_open = &delivered[2];
        assert!(ep2_open.above_warn);
        assert_eq!(
            ep2_open.episode_elevated_ticks,
            Some(2),
            "episode 2 opens fresh (never continuing episode 1's count), deferred one \
             tick by the barrier, so its opening reports 2 elevated ticks"
        );
        assert_eq!(ep2_open.episode_peak_wal_pages, Some(3_500));

        let ep2_recovery = &delivered[3];
        assert!(!ep2_recovery.above_warn);
        assert_eq!(
            ep2_recovery.episode_elevated_ticks,
            Some(2),
            "episode 2's recovery must report only its own 2 elevated ticks"
        );
        assert_eq!(ep2_recovery.episode_peak_wal_pages, Some(3_500));
    }

    /// Degenerate barrier arm: an episode whose entire lifetime falls inside
    /// the window where an earlier recovery is still undelivered is discarded
    /// rather than reported out of order — from any consumer's view it never
    /// opened, so no stale opening or recovery for it may surface after the
    /// queue frees. The loss itself is counted and logged at the discard
    /// site; this test pins the delivered-history shape.
    #[test]
    fn episode_elapsed_entirely_behind_barrier_is_discarded_not_reordered() {
        let config = CheckpointConfig {
            warn_pages: 1_000,
            ..CheckpointConfig::default()
        };
        let ticks = [
            (true, 1_500), // call 0: episode 1 opens — delivered
            (false, 500),  // call 1: episode 1 recovery — DROPPED
            (true, 9_000), // call 2: retry — DROPPED; barrier defers episode 2's opening
            (false, 400),  // call 3: retry — DROPPED; episode 2 discarded behind barrier
            (false, 300),  // call 4: retry — delivered
            (true, 2_500), // call 5: episode 3 opens — delivered
            (false, 200),  // call 6: episode 3 recovery — delivered
        ];

        let delivered = drive_pressure_ticks(&config, &ticks, |idx| matches!(idx, 1..=3));

        let peaks: Vec<_> = delivered
            .iter()
            .map(|payload| (payload.above_warn, payload.episode_peak_wal_pages))
            .collect();
        assert_eq!(
            peaks,
            vec![
                (true, Some(1_500)),  // episode 1 open
                (false, Some(1_500)), // episode 1 delayed recovery
                (true, Some(2_500)),  // episode 3 open — episode 2 (peak 9_000) never surfaces
                (false, Some(2_500)), // episode 3 recovery
            ],
            "an episode elapsed entirely behind the barrier must not surface late or \
             out of order: {delivered:?}"
        );
    }

    /// ASCII-simple control for the regression above: the identical tick
    /// sequence with no queue drops must report the same two episodes
    /// separately (and promptly), confirming the merge in the drop case is
    /// caused by the drop, not by the tick sequence itself.
    #[test]
    fn no_dropped_handoff_reports_two_separate_episodes() {
        let config = CheckpointConfig {
            warn_pages: 1_000,
            ..CheckpointConfig::default()
        };
        let ticks = [
            (true, 1_500),
            (true, 1_800),
            (true, 2_000),
            (false, 500),
            (false, 400),
            (true, 3_000),
            (true, 3_500),
            (false, 300),
        ];

        let delivered = drive_pressure_ticks(&config, &ticks, |_idx| false);

        assert_eq!(delivered.len(), 4, "{delivered:?}");
        assert_eq!(
            (
                delivered[0].above_warn,
                delivered[0].episode_elevated_ticks,
                delivered[0].episode_peak_wal_pages
            ),
            (true, Some(1), Some(1_500)),
            "episode 1 open"
        );
        assert_eq!(
            (
                delivered[1].above_warn,
                delivered[1].episode_elevated_ticks,
                delivered[1].episode_peak_wal_pages
            ),
            (false, Some(3), Some(2_000)),
            "episode 1 recovery"
        );
        assert_eq!(
            (
                delivered[2].above_warn,
                delivered[2].episode_elevated_ticks,
                delivered[2].episode_peak_wal_pages
            ),
            (true, Some(1), Some(3_000)),
            "episode 2 open"
        );
        assert_eq!(
            (
                delivered[3].above_warn,
                delivered[3].episode_elevated_ticks,
                delivered[3].episode_peak_wal_pages
            ),
            (false, Some(2), Some(3_500)),
            "episode 2 recovery"
        );
    }

    #[test]
    #[serial(checkpoint_skip_metrics)]
    fn pressure_diagnostics_count_observations_and_transitions_separately() {
        reset_checkpoint_metrics_for_tests();

        note_checkpoint_pressure_observation(true, false);
        note_checkpoint_pressure_observation(true, true);
        note_checkpoint_pressure_observation(true, true);
        note_checkpoint_pressure_observation(false, true);
        note_checkpoint_pressure_observation(false, false);

        assert_eq!(checkpoint_pressure_elevated_ticks(), 3);
        assert_eq!(checkpoint_pressure_episodes_started(), 1);
        assert_eq!(checkpoint_pressure_episodes_recovered(), 1);
        assert_eq!(checkpoint_lifecycle_append_attempts(), 0);
    }

    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_emits_one_opening_for_persistent_pressure() {
        reset_checkpoint_metrics_for_tests();
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
            Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
            shutdown_rx,
            true,
        ));

        let progressed = wait_for(Duration::from_secs(10), || {
            checkpoint_pressure_elevated_ticks() >= 10
        })
        .await;
        let emitted = wait_for(Duration::from_secs(10), || {
            !store.events.lock().unwrap().is_empty()
        })
        .await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        let events = store.events.lock().unwrap();
        assert!(
            progressed,
            "the simulated persistent-pressure episode must span at least ten checkpoint ticks"
        );
        assert!(
            emitted,
            "an always-elevated config must append one CheckpointOutcomeRecorded event \
             within the poll deadline"
        );
        assert_eq!(
            checkpoint_lifecycle_append_attempts(),
            1,
            "primary-store lifecycle writes must stay O(state transitions), not O(attempts)"
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload_schema_version, 2);
        assert_eq!(events[0].payload["episode_elevated_ticks"], 1);
        assert_eq!(
            events[0].payload["episode_peak_wal_pages"],
            events[0].payload["wal_pages"]
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

    /// Regression #1434/#1838: a lifecycle append may wait five seconds for
    /// its sink writer, while checkpoint observations must continue without
    /// enqueueing one new row per elevated tick.
    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_cycles_and_task_shutdown_do_not_wait_for_a_contended_lifecycle_writer() {
        reset_checkpoint_metrics_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcome_contended_sink.db");
        let checkpoint_pool = file_pool(&path);

        let event_pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                checkout_timeout: Duration::from_secs(5),
                write_queue_enabled: Some(false),
                ..PoolConfig::default()
            })
            .expect("event pool"),
        );
        {
            let writer = event_pool.try_writer().expect("initialize event schema");
            crate::stores::event::ensure_events_schema(writer.conn())
                .expect("initialize event schema");
        }
        let event_store: Arc<dyn khive_storage::EventStore> =
            Arc::new(crate::stores::event::SqlEventStore::new_scoped(
                Arc::clone(&event_pool),
                false,
                "local",
            ));
        let held_event_writer = event_pool
            .try_writer()
            .expect("hold the event-store writer");

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: 0,
            ..CheckpointConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            checkpoint_pool,
            cfg,
            Some(CheckpointLifecycleOwner::new(event_store, "local")),
            shutdown_rx,
            true,
        ));

        let progressed = wait_for(Duration::from_secs(2), || {
            checkpoint_pressure_elevated_ticks() >= 10
        })
        .await;
        assert!(
            progressed,
            "checkpoint observations must continue while the lifecycle append is contended"
        );
        assert_eq!(checkpoint_lifecycle_append_attempts(), 1);
        assert_eq!(checkpoint_lifecycle_enqueue_drops(), 0);

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect(
                "the run_checkpoint_task handle must not wait for the event store's \
                 five-second writer checkout",
            )
            .expect("checkpoint task panicked");

        // The bound above is deliberately checkpoint-task-local. Aborting the
        // lifecycle worker cannot cancel the `spawn_blocking` checkout already
        // admitted by `SqlEventStore`; release its fixture contention only
        // after the `run_checkpoint_task` handle has returned.
        drop(held_event_writer);
    }

    /// A sink error is observable without turning a sustained pressure
    /// episode into a retrying primary-store write loop.
    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn checkpoint_task_continues_after_lifecycle_append_failure() {
        reset_checkpoint_metrics_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcome_failing_sink.db");
        let pool = file_pool(&path);
        let store = Arc::new(FakeEventStore::failing());
        let store_dyn: Arc<dyn khive_storage::EventStore> = store.clone();

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            warn_pages: 0,
            ..CheckpointConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
            shutdown_rx,
            true,
        ));

        let progressed = wait_for(Duration::from_secs(2), || {
            checkpoint_pressure_elevated_ticks() >= 10
        })
        .await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should remain responsive after sink failure")
            .expect("checkpoint task panicked");

        assert!(
            progressed,
            "a failed append must not terminate or stall the checkpoint task"
        );
        assert_eq!(
            store
                .append_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a persistent pressure state must not retry one primary-store append per tick"
        );
        assert_eq!(checkpoint_lifecycle_append_attempts(), 1);
        assert_eq!(checkpoint_lifecycle_append_failures(), 1);
        let captured = buffer.lock().unwrap().clone();
        assert!(
            captured.iter().any(|event| event.message.as_deref()
                == Some("checkpoint lifecycle event append failed")),
            "lifecycle append failures must remain observable; got: {:?}",
            captured
        );
    }

    #[tokio::test]
    #[serial(checkpoint_skip_metrics)]
    async fn secondary_checkpoint_task_with_lifecycle_ownership_emits_outcome_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secondary_outcome.db");
        let pool = file_pool(&path);
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
            Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
            shutdown_rx,
            false,
        ));

        // Poll for the first emitted event instead of a fixed sleep (same
        // slowdown-flake class as the stale-sweep test above).
        let emitted = wait_for(Duration::from_secs(10), || {
            !store.events.lock().unwrap().is_empty()
        })
        .await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        assert!(
            emitted,
            "a designated secondary lifecycle owner must append outcome events within the poll \
             deadline"
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
            Some(CheckpointLifecycleOwner::new(store_dyn, "local")),
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
        let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

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
        let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

        // Poll for the sweep instead of a fixed sleep: a fixed wall-clock
        // budget assumes the spawned task completes its tick (registry scan
        // + walpin sidecar heartbeat) within that window, which widens and
        // flakes under slowdown (coverage instrumentation, contended CI
        // runners) — see `wait_for`'s doc comment for the same reasoning
        // applied to the sibling walpin tests.
        let swept = wait_for(Duration::from_secs(10), || {
            buffer.lock().unwrap().iter().any(|e| {
                e.tx_label.as_deref() == Some("checkpoint_task_healthy_wal_sweep_test")
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.contains("stale-op cap"))
            })
        })
        .await;

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        drop(_tx_handle);

        let events = buffer.lock().unwrap();
        assert!(
            swept,
            "expected the spawned task to sweep and escalate the stale registry entry \
             to Stale on its own within the poll deadline, got: {events:?}"
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
        let handle = tokio::spawn(run_checkpoint_task(pool, cfg, None, shutdown_rx, true));

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

    /// (3) High-finding regression: a Skipped tick must NOT silence the age
    /// sweep. Since the dedicated-connection fix, holding the pool's writer
    /// mutex no longer produces a Skipped tick at all (see
    /// `checkpoint_once_proceeds_and_can_attempt_truncate_while_pool_writer_held`),
    /// so this drives Skipped the way it now actually happens in production:
    /// a read-only pool, on which `ConnectionPool::open_standalone_writer`
    /// always fails, so `CheckpointConnection::ensure_open` can never open a
    /// dedicated connection and every tick reports `Skipped`. Asserts the age
    /// alert still fires across several such ticks alongside a stale
    /// registered entry. Before the original fix (#845 predecessor), the
    /// sweep call sat after the `Skipped` early-continue and never ran here;
    /// this regression must keep holding under the new skip mechanism too.
    #[tokio::test]
    #[serial(tx_registry, checkpoint_skip_metrics)]
    async fn checkpoint_task_sweeps_stale_entry_even_when_dedicated_connection_is_unavailable_every_tick(
    ) {
        reset_checkpoint_metrics_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx_age_sweep_task_conn_unavailable.db");
        {
            // Seed the schema with an ordinary read-write pool, then drop it
            // (releasing its connections) before reopening the same file
            // read-only below.
            let seed_pool = file_pool(&path);
            let writer = seed_pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);")
                .unwrap();
        }

        #[cfg(unix)]
        {
            khive_storage::test_support::freeze_snapshot_sidecars(&path);
        }

        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path.clone()),
                read_only: true,
                ..PoolConfig::default()
            })
            .expect("read-only pool open"),
        );
        assert!(
            pool.open_standalone_writer().is_err(),
            "test precondition: a read-only pool must never be able to open a dedicated \
             checkpoint connection"
        );

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let _tx_handle = khive_storage::tx_registry::register(Some(
            "checkpoint_task_conn_unavailable_sweep_test".to_string(),
        ));

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
            shutdown_rx,
            true,
        ));

        // Wait until the task has actually recorded a Skipped tick rather
        // than sleeping a fixed real-time budget: each tick also does
        // registry queries and sidecar filesystem writes, so under
        // instrumented (coverage) or loaded runners a fixed sleep races the
        // first completed tick. Bounded, fail-loud.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while checkpoint_skipped_ticks() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "test setup must actually drive at least one Skipped tick for this \
                 regression to mean anything (none within 10s)"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        loop {
            let events = buffer.lock().unwrap().clone();
            if events.iter().any(|e| {
                e.tx_label.as_deref() == Some("checkpoint_task_conn_unavailable_sweep_test")
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.contains("stale-op cap"))
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected the age sweep to fire even though every tick's dedicated \
                 connection was unavailable within 10s, got: {events:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");
        drop(_tx_handle);
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
        let pid = std::process::id();
        state.register_beacon().await;
        let beacon_path = sidecar_dir.join(format!("{pid}.beacon"));
        let before = std::fs::metadata(&beacon_path)
            .expect("register_beacon must create the beacon file")
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
    #[serial(khive_walpin_sidecar_env)]
    async fn walpin_observe_touches_mtime_without_rewriting_body_when_content_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("observe_touch.db");
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
        let pid = std::process::id();
        let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
        let span = khive_storage::tx_registry::OldestSpan {
            id: khive_storage::tx_registry::TxId(1),
            age: Duration::from_secs(60),
            label: None,
            origin: khive_storage::tx_registry::TxOrigin::Unscoped,
        };

        state
            .observe(Some(span.clone()), Duration::from_secs(30))
            .await;
        let body_after_create = std::fs::read(&heartbeat_path).expect("heartbeat written");

        // Backdate the mtime so the touch is unambiguous: if `observe`
        // rewrote the body instead of touching it, the write would also
        // reset the mtime, making this assertion pass for the wrong reason —
        // the body-byte comparison below is what actually distinguishes
        // touch from rewrite.
        let backdated = std::time::SystemTime::now() - Duration::from_secs(120);
        // `set_modified` needs write access to the handle on Windows (a
        // read-only open succeeds but is refused by `set_modified` with
        // `PermissionDenied`); Unix accepts a read-only handle for this.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&heartbeat_path)
            .unwrap()
            .set_modified(backdated)
            .unwrap();

        state.observe(Some(span), Duration::from_secs(30)).await;

        let body_after_second_observe =
            std::fs::read(&heartbeat_path).expect("heartbeat still present");
        assert_eq!(
            body_after_create, body_after_second_observe,
            "unchanged oldest-span identity/label/attribution/cadence must touch mtime, \
             not rewrite the body"
        );
        let mtime_after = std::fs::metadata(&heartbeat_path)
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            mtime_after > backdated,
            "the touch must advance mtime past the backdated value"
        );
    }

    #[tokio::test]
    #[serial(khive_walpin_sidecar_env)]
    async fn walpin_observe_recreates_heartbeat_after_it_is_deleted_while_span_still_live() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("observe_recreate.db");
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
        let pid = std::process::id();
        let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
        let span = khive_storage::tx_registry::OldestSpan {
            id: khive_storage::tx_registry::TxId(1),
            age: Duration::from_secs(60),
            label: None,
            origin: khive_storage::tx_registry::TxOrigin::Unscoped,
        };

        state
            .observe(Some(span.clone()), Duration::from_secs(30))
            .await;
        assert!(heartbeat_path.exists(), "heartbeat written on first tick");

        // Simulate enumeration deleting a slow writer's heartbeat while its
        // span is still live: the next tick still sees unchanged content
        // (same span, same label, same attribution, same cadence) so it
        // takes the touch path — which must detect the missing target and
        // fall through to a full recreate rather than silently no-op.
        std::fs::remove_file(&heartbeat_path).unwrap();
        assert!(!heartbeat_path.exists());

        state.observe(Some(span), Duration::from_secs(30)).await;

        assert!(
            heartbeat_path.exists(),
            "a touch failure against a deleted heartbeat must recreate it via a full write"
        );
        let recreated: crate::walpin::WalpinHeartbeat =
            serde_json::from_slice(&std::fs::read(&heartbeat_path).unwrap()).unwrap();
        assert_eq!(recreated.pid, pid);
        assert_eq!(recreated.oldest_tx_age_secs, 60.0);
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
        let beacon_registered = wait_for(Duration::from_secs(2), || beacon.exists()).await;
        assert!(
            beacon_registered,
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

    /// ADR-091 Amendment 3: a `run_checkpoint_task` instance for backend A
    /// (`is_main: false`, a `Secondary` filter scoped to A's own identity)
    /// must never observe a span registered against a DIFFERENT backend's
    /// `Database` origin, nor an `Unscoped` span — a `Secondary` filter never
    /// falls back to `Unscoped` (that fallback is the main view's alone).
    /// Drives the real task for several ticks and asserts neither the
    /// captured `tracing` emissions nor backend A's own sidecar ever name
    /// either span.
    #[tokio::test]
    #[serial(tx_registry, checkpoint_skip_metrics, khive_walpin_sidecar_env)]
    async fn checkpoint_task_ignores_span_registered_against_other_backend_origin_and_unscoped() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let pool_a = file_pool(&dir_a.path().join("backend_a.db"));
        // Only used to mint a real, distinct `DbIdentity` for backend B — no
        // checkpoint task is spawned for it.
        let pool_b = file_pool(&dir_b.path().join("backend_b.db"));
        let sidecar_a =
            crate::walpin::sidecar_dir_for(pool_a.canonical_path().expect("file-backed"));
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let _b_origin_handle = khive_storage::tx_registry::register_scoped(
            Some("b_origin_span_ignored_by_a".to_string()),
            pool_b.origin(),
        );
        let _unscoped_handle = khive_storage::tx_registry::register(Some(
            "unscoped_span_ignored_by_secondary".to_string(),
        ));

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            tx_warn_secs: Duration::from_millis(1),
            tx_max_age_secs: Duration::from_millis(1),
            ..CheckpointConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(run_checkpoint_task(
            pool_a,
            cfg,
            None,
            shutdown_rx,
            false, // is_main: backend A is a secondary backend here
        ));

        // No positive condition to poll for — this asserts an absence over a
        // bounded run of several ticks, mirroring
        // `checkpoint_task_emits_no_age_alert_for_an_empty_registry`'s same
        // fixed-window shape (there is nothing to wait-until for a negative).
        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().all(|e| {
                e.tx_label.as_deref() != Some("b_origin_span_ignored_by_a")
                    && e.tx_label.as_deref() != Some("unscoped_span_ignored_by_secondary")
            }),
            "backend A's Secondary filter must never emit an age alert naming a span \
             registered against a different backend's origin or an Unscoped span, got: \
             {events:?}"
        );
        assert!(
            !sidecar_a
                .join(format!("{}.json", std::process::id()))
                .exists(),
            "backend A's own sidecar must never gain a heartbeat from a span it does not own"
        );
    }

    /// ADR-091 Amendment 3: a secondary backend's own `run_checkpoint_task`
    /// must detect a stall on its OWN backend (never main-only ownership) —
    /// both the Plank 1 age-sweep emission and the sidecar heartbeat, with
    /// `attribution_basis="origin"` (a `Secondary` filter winner is always
    /// `Database`-origin-backed, never the `Unscoped` fallback) and a
    /// nonzero reflected age.
    #[tokio::test]
    #[serial(tx_registry, checkpoint_skip_metrics, khive_walpin_sidecar_env)]
    async fn checkpoint_task_detects_and_enumerates_secondary_backend_stall() {
        let dir = tempfile::tempdir().unwrap();
        let pool = file_pool(&dir.path().join("secondary_stall.db"));
        let sidecar_dir =
            crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed"));
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "1");

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: std::sync::Arc::clone(&buffer),
        };
        let _tracing_guard = tracing::subscriber::set_default(subscriber);

        let tx_handle = khive_storage::tx_registry::register_scoped(
            Some("secondary_stall_test".to_string()),
            pool.origin(),
        );

        let cfg = CheckpointConfig {
            interval: Duration::from_millis(10),
            tx_warn_secs: Duration::from_millis(5),
            tx_max_age_secs: Duration::from_millis(500),
            ..CheckpointConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let pid = std::process::id();
        let heartbeat_path = sidecar_dir.join(format!("{pid}.json"));
        let handle = tokio::spawn(run_checkpoint_task(
            pool,
            cfg,
            None,
            shutdown_rx,
            false, // is_main: this is a secondary backend's own checkpoint task
        ));

        assert!(
            wait_for(Duration::from_secs(2), || heartbeat_path.exists()).await,
            "expected a walpin heartbeat once the secondary backend's own span crossed \
             tx_warn_secs"
        );
        let body = std::fs::read_to_string(&heartbeat_path).unwrap();
        let hb: crate::walpin::WalpinHeartbeat = serde_json::from_str(&body).unwrap();
        assert_eq!(hb.oldest_tx_label.as_deref(), Some("secondary_stall_test"));
        assert_eq!(
            hb.attribution_basis.as_deref(),
            Some("origin"),
            "a Secondary-view winner is always Database-origin-backed — never fallback"
        );
        assert!(
            hb.oldest_tx_age_secs > 0.0,
            "the heartbeat must reflect a nonzero stale age for the secondary backend's own \
             span, got {hb:?}"
        );

        shutdown_tx.send(()).expect("send shutdown signal");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("checkpoint task should exit within 1s")
            .expect("checkpoint task panicked");

        drop(tx_handle);

        let events = buffer.lock().unwrap();
        assert!(
            events.iter().any(|e| {
                e.tx_label.as_deref() == Some("secondary_stall_test")
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.contains("ADR-091 Plank 1"))
            }),
            "expected the secondary backend's own checkpoint task to emit a Plank 1 age alert \
             for its own stalled span, got: {events:?}"
        );
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

    /// #1849: a canonical filesystem identity is an OS path, not a display
    /// label. Distinct non-UTF-8 Unix paths can render to the same lossy
    /// string and must still occupy distinct backend telemetry slots.
    #[cfg(unix)]
    #[test]
    fn routine_wal_backend_key_preserves_non_utf8_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path_a = PathBuf::from(OsString::from_vec(b"/tmp/khive-wal-\x80.db".to_vec()));
        let path_b = PathBuf::from(OsString::from_vec(b"/tmp/khive-wal-\x81.db".to_vec()));
        assert_eq!(
            path_a.display().to_string(),
            path_b.display().to_string(),
            "fixture must reproduce the lossy display-label collision"
        );
        assert_ne!(
            checkpoint_db_key_from_path(Some(&path_a)),
            checkpoint_db_key_from_path(Some(&path_b)),
            "backend keys must retain the canonical path's exact OS bytes"
        );
    }

    /// #1849: the periodic checkpoint's own PASSIVE row is the monitoring
    /// sample. One tick must not issue the old no-arg probe followed by a
    /// second PASSIVE, and the stored sample must distinguish logical
    /// backlog from the physical sidecar high-water mark.
    #[test]
    #[serial(checkpoint_skip_metrics)]
    fn routine_checkpoint_records_one_pass_logical_and_physical_wal_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routine_wal_sample.db");
        let pool = file_pool(&path);

        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch(
                    "PRAGMA wal_autocheckpoint=0; \
                     CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT); \
                     INSERT INTO t VALUES (0, 'seed');",
                )
                .unwrap();
        }

        let reader = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .unwrap();

        {
            let writer = pool.try_writer().expect("writer");
            writer.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
            for id in 1..=256_i64 {
                writer
                    .conn()
                    .execute("INSERT INTO t VALUES (?1, printf('%.*c', 2048, 'x'))", [id])
                    .unwrap();
            }
            writer.conn().execute_batch("COMMIT").unwrap();
        }

        let checkpoint_conn = pool.open_standalone_writer().unwrap();
        let pragma_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pragma_calls_from_hook = Arc::clone(&pragma_calls);
        checkpoint_conn
            .authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Pragma { pragma_name, .. }
                        if pragma_name.eq_ignore_ascii_case("wal_checkpoint")
                ) {
                    pragma_calls_from_hook.fetch_add(1, Ordering::SeqCst);
                }
                Authorization::Allow
            }))
            .unwrap();

        checkpoint_once(
            &pool,
            &checkpoint_conn,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        )
        .unwrap();
        checkpoint_conn
            .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
            .unwrap();

        assert_eq!(
            pragma_calls.load(Ordering::SeqCst),
            1,
            "one routine tick must issue exactly one PASSIVE checkpoint"
        );
        let pinned = routine_wal_observation(&pool).expect("routine sample");
        assert!(pinned.log_frames > 0, "the test must create WAL frames");
        assert!(
            pinned.pending_frames > 0,
            "the old reader must leave a logical backlog: {pinned:?}"
        );
        assert_eq!(
            pinned.pending_frames,
            pinned.log_frames.saturating_sub(pinned.checkpointed_frames)
        );
        assert!(
            pinned.physical_wal_bytes.is_some_and(|bytes| bytes > 0),
            "the physical sidecar high-water must be reported separately: {pinned:?}"
        );

        reader.execute_batch("COMMIT").unwrap();
        checkpoint_once(
            &pool,
            &checkpoint_conn,
            &CheckpointConfig::default(),
            &mut TruncateState::default(),
        )
        .unwrap();
        let drained = routine_wal_observation(&pool).expect("drained routine sample");
        assert_eq!(drained.pending_frames, 0, "unpinned PASSIVE must drain");
        assert!(
            drained.physical_wal_bytes.is_some_and(|bytes| bytes > 0),
            "PASSIVE may reuse rather than shrink the physical WAL; the two gauges must remain \
             independently visible: {drained:?}"
        );
    }
}
