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
//! `CheckpointTick::Skipped` and the next tick lazily reopens it. A busy or
//! inconsistent PASSIVE result also skips pressure decisions without replacing
//! the last valid WAL sample; a busy pool writer does not cause either skip.
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
//!
//! The same long-lived standalone connection also owns a separate, five-minute
//! FTS5 maintenance cadence. One due call gives one index at most 500 pages of
//! incremental merge work and uses a zero busy timeout, so this best-effort
//! derived-index maintenance cannot queue behind application writes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pool::ConnectionPool;

mod backfill_queries;
mod checkpoint_core;
mod lifecycle;
mod task_loop;
mod walpin_attribution;
mod walpin_sidecar;

use backfill_queries::{
    crossing_warn, log_backfill_gap, observed_wal_pages, query_routine_checkpoint_observation,
    query_truncate_observation, query_wal_pages, CheckpointUnavailableReason,
    RawCheckpointObservation,
};
#[cfg(test)]
use backfill_queries::{
    query_backfill_gap, query_checkpoint_observation, test_arm_passive_row, test_take_passive_row,
};
pub(crate) use checkpoint_core::{checkpoint_once_core, CheckpointCoreOutcome};
#[cfg(test)]
use checkpoint_core::{
    log_truncate_no_progress_warn, maybe_truncate, note_truncate_outcome,
    truncate_needs_attribution, truncate_report_test_sync,
};
use checkpoint_core::{
    log_tx_registry_oldest_debug, log_tx_registry_oldest_warn, log_tx_registry_snapshot_warn,
    log_wal_high_water_warn,
};

pub use checkpoint_core::checkpoint_once;
pub use lifecycle::{run_session_sweep_task, CheckpointLifecycleOwner, SweepBackend};
pub use task_loop::run_checkpoint_task;

#[cfg(test)]
pub(crate) fn truncate_requests_attribution_for_test(
    pool: &crate::pool::ConnectionPool,
    conn: &rusqlite::Connection,
    config: &CheckpointConfig,
    wal_pages_before: u64,
    state: &mut TruncateState,
) -> bool {
    checkpoint_core::maybe_truncate(pool, conn, config, wal_pages_before, state).is_some()
}

use lifecycle::{CheckpointConnection, CheckpointLifecycleEmitter, CheckpointPressureEpisode};
#[cfg(test)]
use task_loop::{
    checkpoint_outcome_should_emit, observe_checkpoint_pressure_tick,
    run_fts_maintenance_off_worker,
};
use walpin_attribution::WalpinAttributionRequest;
#[cfg(unix)]
use walpin_attribution::{capture_walpin_attribution_request, complete_walpin_attribution};
#[cfg(all(test, unix))]
use walpin_attribution::{
    log_walpin_sidecar_report, walpin_attribution_test_sync, WalpinAttributionFailure,
    WalpinReportFreshness,
};
use walpin_sidecar::{log_tx_age_emission, ReaderCheckoutSweepState, WalpinSidecarState};
#[cfg(unix)]
use walpin_sidecar::{
    now_epoch_secs, run_walpin_housekeeping_if_due, CachedWalpinAttribution, WalpinFullScanPlan,
    DEFAULT_WALPIN_FULL_SCAN_INTERVAL,
};

mod off_worker;
#[cfg(test)]
mod off_worker_tests;

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

/// Count of checkpoint ticks without a usable WAL frame observation because
/// the dedicated connection was unavailable or SQLite returned a busy or
/// inconsistent PASSIVE row.
/// Never reset outside `#[cfg(test)]`.
static CHECKPOINT_SKIPPED_TICKS: AtomicU64 = AtomicU64::new(0);

/// Current run-length of consecutive skipped ticks. Reset to 0 the next time
/// a tick has a valid WAL frame observation, so a
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

mod run_state;

pub use run_state::{
    checkpoint_consecutive_skips, checkpoint_last_skip_wal_pages,
    checkpoint_lifecycle_append_attempts, checkpoint_lifecycle_append_failures,
    checkpoint_lifecycle_enqueue_drops, checkpoint_pressure_elevated_ticks,
    checkpoint_pressure_episodes_recovered, checkpoint_pressure_episodes_started,
    checkpoint_skipped_ticks, checkpoint_timing, last_observed_wal_pages,
    read_tx_max_age_evictions, routine_wal_observation, truncate_attempts,
    truncate_consecutive_failures, CheckpointRun, CheckpointTick, CheckpointTiming,
    RoutineWalObservation,
};

pub(crate) use run_state::{
    checkpoint_run_snapshot, note_read_tx_max_age_eviction, record_checkpoint_run_result,
    CheckpointRunStatus, CheckpointRunTaskGuard,
};

use run_state::{
    note_checkpoint_observed, note_checkpoint_pressure_observation, note_checkpoint_skipped,
    record_checkpoint_timing, record_routine_wal_observation,
};

#[cfg(test)]
pub(crate) use run_state::{checkpoint_run_status, reset_checkpoint_metrics_for_tests};

#[cfg(test)]
use run_state::{
    advance_checkpoint_run, advance_checkpoint_run_at, checkpoint_db_key,
    checkpoint_db_key_from_path, checkpoint_runs, checkpoint_timings,
};

mod config;

pub(crate) use config::tx_age_thresholds_from_env;
pub use config::{CheckpointConfig, DEFAULT_WARN_SUSTAINED_CYCLES};

mod severity_state;

pub use severity_state::{
    CheckpointSeverityEmission, CheckpointSeverityRung, CheckpointSeverityState, TruncateState,
    TxAgeEmission, TxAgeRung, TxAgeSweepState,
};

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
    let millis = crate::env::env_parse_or("KHIVE_SESSION_SWEEP_INTERVAL_MS", 0_u64);
    if millis > 0 {
        Duration::from_millis(millis)
    } else {
        DEFAULT_SESSION_SWEEP_INTERVAL
    }
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
