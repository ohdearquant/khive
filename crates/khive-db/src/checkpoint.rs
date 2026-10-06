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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pool::ConnectionPool;

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

/// One backend's current checkpoint run: informative checkpoint results that
/// stopped at the same frame while frames remained to backfill, allowing
/// bounded neutral busy results between them. Reported by `db_diagnostics` as
/// `oldest_pinned_frame_run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CheckpointRun {
    /// The checkpointed frame every result in the run stopped at.
    pub frame: i64,
    /// Unix time in milliseconds of the run's first checkpoint result.
    pub first_observed_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointRunStatus {
    NoTask,
    NoObservation,
    Observed(CheckpointRun),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckpointRunEntry {
    run: CheckpointRun,
    first_observed_at: Instant,
    last_log_frames: i64,
    last_informative_at: Instant,
    busy_since_last_informative: bool,
}

#[derive(Debug, Default)]
struct CheckpointRunState {
    active_tasks: usize,
    checkpoint_interval_ms: u64,
    owner_intervals_ms: BTreeMap<u64, usize>,
    entry: Option<CheckpointRunEntry>,
}

static CHECKPOINT_RUNS: OnceLock<Mutex<HashMap<Option<PathBuf>, CheckpointRunState>>> =
    OnceLock::new();

fn checkpoint_runs() -> &'static Mutex<HashMap<Option<PathBuf>, CheckpointRunState>> {
    CHECKPOINT_RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) struct CheckpointRunTaskGuard {
    key: Option<PathBuf>,
    interval_ms: u64,
}

impl CheckpointRunTaskGuard {
    pub(crate) fn start(pool: &ConnectionPool, interval: Duration) -> Self {
        let key = checkpoint_db_key(pool);
        let interval_ms = interval.as_millis().min(u128::from(u64::MAX)) as u64;
        let interval_ms = interval_ms.max(1);
        let mut runs = checkpoint_runs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = runs.entry(key.clone()).or_default();
        if state.active_tasks == 0 {
            state.entry = None;
        }
        *state.owner_intervals_ms.entry(interval_ms).or_default() += 1;
        state.active_tasks = state.active_tasks.saturating_add(1);
        state.checkpoint_interval_ms = *state
            .owner_intervals_ms
            .first_key_value()
            .expect("active owner has an interval")
            .0;
        Self { key, interval_ms }
    }
}

impl Drop for CheckpointRunTaskGuard {
    fn drop(&mut self) {
        let mut runs = checkpoint_runs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = runs.get_mut(&self.key) else {
            return;
        };
        let Some(count) = state.owner_intervals_ms.get_mut(&self.interval_ms) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            state.owner_intervals_ms.remove(&self.interval_ms);
        }
        state.active_tasks = state.active_tasks.saturating_sub(1);
        if state.active_tasks == 0 {
            runs.remove(&self.key);
        } else {
            state.checkpoint_interval_ms = *state
                .owner_intervals_ms
                .first_key_value()
                .expect("surviving owner has an interval")
                .0;
        }
    }
}

fn advance_checkpoint_run_at(
    entry: &mut Option<CheckpointRunEntry>,
    observation: Option<(i64, i64, i64)>,
    observed_at_unix_ms: u64,
    observed_at: Instant,
    checkpoint_interval_ms: u64,
) {
    let Some((busy, log_frames, checkpointed_frames)) = observation else {
        *entry = None;
        return;
    };
    // A busy row has no reliable reading of the ceiling, even if SQLite fills
    // in the other columns. It cannot advance or end the current run.
    if busy != 0 {
        if let Some(current) = entry {
            current.busy_since_last_informative = true;
        }
        return;
    }
    if log_frames < 0 || checkpointed_frames < 0 || checkpointed_frames >= log_frames {
        *entry = None;
        return;
    }

    match entry {
        Some(current)
            if current.run.frame == checkpointed_frames
                && log_frames >= current.last_log_frames
                && (!current.busy_since_last_informative
                    || observed_at
                        .checked_duration_since(current.last_informative_at)
                        .is_some_and(|elapsed| {
                            elapsed
                                <= Duration::from_millis(checkpoint_interval_ms.saturating_mul(2))
                        })) =>
        {
            current.last_log_frames = log_frames;
            current.last_informative_at = observed_at;
            current.busy_since_last_informative = false;
        }
        _ => {
            *entry = Some(CheckpointRunEntry {
                run: CheckpointRun {
                    frame: checkpointed_frames,
                    first_observed_at_unix_ms: observed_at_unix_ms,
                },
                first_observed_at: observed_at,
                last_log_frames: log_frames,
                last_informative_at: observed_at,
                busy_since_last_informative: false,
            });
        }
    }
}

#[cfg(test)]
fn advance_checkpoint_run(
    entry: &mut Option<CheckpointRunEntry>,
    observation: Option<(i64, i64, i64)>,
    observed_at_unix_ms: u64,
    checkpoint_interval_ms: u64,
) {
    static TEST_ORIGIN: OnceLock<Instant> = OnceLock::new();
    let observed_at = TEST_ORIGIN
        .get_or_init(Instant::now)
        .checked_add(Duration::from_millis(observed_at_unix_ms))
        .expect("test monotonic timestamp");
    advance_checkpoint_run_at(
        entry,
        observation,
        observed_at_unix_ms,
        observed_at,
        checkpoint_interval_ms,
    );
}

pub(crate) fn record_checkpoint_run_result(
    pool: &ConnectionPool,
    observation: Option<(i64, i64, i64)>,
) -> CheckpointRunStatus {
    let mut runs = checkpoint_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = runs.get_mut(&checkpoint_db_key(pool)) else {
        return CheckpointRunStatus::NoTask;
    };
    if state.active_tasks == 0 {
        return CheckpointRunStatus::NoTask;
    }
    let checkpoint_interval_ms = state.checkpoint_interval_ms;
    advance_checkpoint_run_at(
        &mut state.entry,
        observation,
        observed_at_unix_ms(),
        Instant::now(),
        checkpoint_interval_ms,
    );
    state
        .entry
        .map_or(CheckpointRunStatus::NoObservation, |entry| {
            CheckpointRunStatus::Observed(entry.run)
        })
}

#[cfg(test)]
pub(crate) fn checkpoint_run_status(pool: &ConnectionPool) -> CheckpointRunStatus {
    checkpoint_run_snapshot(pool).0
}

/// A run and its monotonic age from the same state snapshot. The Unix timestamp
/// on `CheckpointRun` is display metadata and must not gate pin detection.
pub(crate) fn checkpoint_run_snapshot(
    pool: &ConnectionPool,
) -> (CheckpointRunStatus, Option<Duration>) {
    let runs = checkpoint_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = runs.get(&checkpoint_db_key(pool)) else {
        return (CheckpointRunStatus::NoTask, None);
    };
    if state.active_tasks == 0 {
        return (CheckpointRunStatus::NoTask, None);
    }
    state
        .entry
        .map_or((CheckpointRunStatus::NoObservation, None), |entry| {
            (
                CheckpointRunStatus::Observed(entry.run),
                Some(entry.first_observed_at.elapsed()),
            )
        })
}

/// Process-lifetime totals for actual routine PASSIVE calls on one store.
/// Ticks with no PASSIVE call and post-TRUNCATE probes are excluded. Busy counts only
/// SQLite's returned busy flag, never an incomplete checkpoint's pending frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CheckpointTiming {
    pub ticks: u64,
    pub elapsed_us_sum: u64,
    pub elapsed_us_max: u64,
    pub busy_ticks: u64,
    pub error_ticks: u64,
}

static CHECKPOINT_TIMINGS: OnceLock<Mutex<HashMap<Option<PathBuf>, CheckpointTiming>>> =
    OnceLock::new();

fn checkpoint_timings() -> &'static Mutex<HashMap<Option<PathBuf>, CheckpointTiming>> {
    CHECKPOINT_TIMINGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_checkpoint_timing(pool: &ConnectionPool, elapsed_us: u64, busy: Option<i64>) {
    let mut timings = checkpoint_timings()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let timing = timings.entry(checkpoint_db_key(pool)).or_default();
    timing.ticks = timing.ticks.saturating_add(1);
    timing.elapsed_us_sum = timing.elapsed_us_sum.saturating_add(elapsed_us);
    timing.elapsed_us_max = timing.elapsed_us_max.max(elapsed_us);
    timing.busy_ticks = timing
        .busy_ticks
        .saturating_add(u64::from(busy.is_some_and(|value| value != 0)));
    timing.error_ticks = timing.error_ticks.saturating_add(u64::from(busy.is_none()));
}

/// Pure in-memory read; zero means no routine call has been recorded for this key.
pub fn checkpoint_timing(pool: &ConnectionPool) -> CheckpointTiming {
    checkpoint_timings()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&checkpoint_db_key(pool))
        .copied()
        .unwrap_or_default()
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

/// Current measured TRUNCATE-failure streak; unmeasured attempts leave it unchanged.
pub fn truncate_consecutive_failures() -> u64 {
    TRUNCATE_CONSECUTIVE_FAILURES.load(Ordering::Relaxed)
}

/// Total checkpoint ticks without an available WAL frame observation in this
/// process's lifetime (dedicated connection unavailable or SQLite busy).
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

/// A tick had no usable WAL frame observation: bump the
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

/// A tick had a valid WAL frame observation: close out any prior skip
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
/// `Skipped` is returned when the dedicated connection is unavailable or its
/// PASSIVE result is busy or inconsistent and has no usable WAL frame
/// observation. A concurrent pool writer does not cause a skip: the task never
/// checks out its writer mutex. `Observed` carries the WAL page count read
/// during the tick. A skipped tick leaves threshold-crossing state unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTick {
    /// No usable WAL frame observation was available this tick.
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
    /// only that the WAL is not draining. Whether an old snapshot is pinning it
    /// is informed at the crossing by the in-process transaction registry,
    /// against `tx_warn_secs` — see `log_wal_high_water_warn`. This registry
    /// cannot exclude readers in another process. Either way an
    /// operator can schedule a blocking TRUNCATE at a safe moment outside
    /// normal write traffic; the two cases differ in what else is worth doing.
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
    /// Count of measured TRUNCATE outcomes that failed to bring `wal_pages`
    /// below `warn_pages`, ignoring attempts whose post-TRUNCATE measurement
    /// was unavailable. A measured clearing result resets it; a one-shot
    /// escalated WARN fires at exactly 3 failures.
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
/// one. A busy or inconsistent PASSIVE result also skips a tick while keeping
/// this connection open.
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
}

/// Run one due FTS5 maintenance step off this task's Tokio worker thread.
///
/// A due step can issue up to `config.merge_pages` pages of synchronous
/// SQLite incremental-merge I/O against a trigram index over a corpus of
/// hundreds of thousands of rows — the same class of blocking work the
/// WAL-pin beacon writes above already move off the worker via
/// `tokio::task::spawn_blocking`. `conn` and `state` are moved into the
/// blocking closure and handed back to the caller whenever the step returns,
/// so the checkpoint task can restore its dedicated connection and scheduler
/// state on every non-panicking path. A panic inside the step surfaces as the
/// `JoinError`, like the beacon writes above: the connection and scheduler
/// state moved into the task are gone with it, and the caller reopens both
/// on the next tick instead of taking the checkpoint task down.
async fn run_fts_maintenance_off_worker(
    conn: rusqlite::Connection,
    config: crate::fts_maintenance::FtsMaintenanceConfig,
    mut state: crate::fts_maintenance::FtsMaintenanceState,
    now: Instant,
) -> Result<
    (
        rusqlite::Connection,
        crate::fts_maintenance::FtsMaintenanceState,
        Result<Option<crate::fts_maintenance::FtsMaintenanceStep>, String>,
    ),
    tokio::task::JoinError,
> {
    tokio::task::spawn_blocking(move || {
        let result = crate::fts_maintenance::run_if_due(&conn, &config, &mut state, now);
        (conn, state, result)
    })
    .await
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
/// `truncate_busy_timeout` (see `CheckpointConnection`'s contract). The
/// checkpoint call itself runs on `spawn_blocking`, so that wait never holds
/// one of the runtime's worker threads. A tick is
/// `Skipped` when that connection is unavailable or SQLite returns a busy
/// PASSIVE row without a usable pressure observation. A
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
    let _checkpoint_run_guard = CheckpointRunTaskGuard::start(&pool, config.interval);
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
    // FTS5 segment maintenance shares this task's standalone connection, but
    // not its 500 ms cadence. Each due call performs at most one bounded
    // merge step on one table and refuses immediately when another writer
    // owns SQLite's write lock.
    let mut fts_maintenance_config = crate::fts_maintenance::FtsMaintenanceConfig::from_env();
    // Secondary backends have independent schemas (for example the code-map
    // database) and are not required to contain the substrate FTS tables.
    // Exactly the main backend owns this derived-index maintenance.
    fts_maintenance_config.enabled &= is_main;
    let mut fts_maintenance_state =
        crate::fts_maintenance::FtsMaintenanceState::new(Instant::now());

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

        let tick = if checkpoint_conn.ensure_open(&pool).is_none() {
            note_checkpoint_skipped();
            CheckpointTick::Skipped
        } else {
            // `ensure_open` above just confirmed a connection is open. Take
            // ownership of it so the checkpoint cycle and the FTS maintenance
            // step below can each move it onto a blocking thread; every path
            // either restores it to `checkpoint_conn` or lets it drop, which is
            // the moved-ownership equivalent of the former `drop_connection()`
            // call.
            let conn = checkpoint_conn
                .conn
                .take()
                .expect("ensure_open just confirmed a connection is open");
            match off_worker::run_checkpoint_core_off_worker(
                Arc::clone(&pool),
                conn,
                config.clone(),
                truncate_state,
            )
            .await
            {
                Ok((conn, state, Ok(outcome))) => {
                    truncate_state = state;
                    #[cfg(unix)]
                    {
                        pending_sidecar_attribution = outcome.sidecar_attribution;
                    }
                    #[cfg(not(unix))]
                    let _ = outcome.sidecar_attribution;

                    // Only a due tick moves the connection onto a blocking
                    // thread; an ordinary tick between maintenance intervals
                    // keeps it here and records a no-op.
                    let fts_result = if !fts_maintenance_state
                        .is_due(&fts_maintenance_config, Instant::now())
                    {
                        checkpoint_conn.conn = Some(conn);
                        Ok(None)
                    } else {
                        match run_fts_maintenance_off_worker(
                            conn,
                            fts_maintenance_config.clone(),
                            fts_maintenance_state,
                            Instant::now(),
                        )
                        .await
                        {
                            Ok((conn, state, fts_result)) => {
                                fts_maintenance_state = state;
                                checkpoint_conn.conn = Some(conn);
                                fts_result
                            }
                            Err(join_err) => {
                                // The step panicked on the blocking thread. The
                                // connection and scheduler state moved into it
                                // are gone; `ensure_open` reopens the connection
                                // next tick and the schedule restarts from now.
                                // The checkpoint pragma itself already succeeded.
                                fts_maintenance_state =
                                    crate::fts_maintenance::FtsMaintenanceState::new(Instant::now());
                                Err(format!(
                                    "bounded FTS5 segment maintenance task panicked: {join_err}"
                                ))
                            }
                        }
                    };

                    match fts_result {
                        Ok(Some(step)) => match step.outcome {
                            crate::fts_maintenance::FtsMaintenanceOutcome::Worked => {
                                tracing::info!(
                                    table = step.table,
                                    requested_pages = step.requested_pages,
                                    segments_before = step.segments_before,
                                    segments_after = step.segments_after,
                                    "bounded FTS5 segment maintenance made progress"
                                );
                            }
                            crate::fts_maintenance::FtsMaintenanceOutcome::Busy => {
                                tracing::debug!(
                                    table = step.table,
                                    requested_pages = step.requested_pages,
                                    segments = step.segments_before,
                                    "bounded FTS5 segment maintenance skipped a busy writer"
                                );
                            }
                            crate::fts_maintenance::FtsMaintenanceOutcome::Noop
                            | crate::fts_maintenance::FtsMaintenanceOutcome::BelowThreshold => {
                                tracing::debug!(
                                    table = step.table,
                                    outcome = ?step.outcome,
                                    segments = step.segments_before,
                                    "bounded FTS5 segment maintenance had no work"
                                );
                            }
                        },
                        Ok(None) => {}
                        Err(error) => {
                            // The checkpoint pragma already succeeded. An FTS
                            // structure/read/merge error is an independent,
                            // best-effort maintenance failure and must not make
                            // the task discard an otherwise healthy connection.
                            tracing::warn!(
                                error = %error,
                                "bounded FTS5 segment maintenance failed"
                            );
                        }
                    }
                    match outcome.wal_pages {
                        Some(wal_pages) => CheckpointTick::Observed(wal_pages),
                        None => {
                            note_checkpoint_skipped();
                            CheckpointTick::Skipped
                        }
                    }
                }
                Ok((_conn, state, Err(e))) => {
                    truncate_state = state;
                    tracing::warn!(
                        error = %e,
                        "dedicated checkpoint connection failed a pragma; \
                         dropping it for a fresh reopen next tick"
                    );
                    note_checkpoint_skipped();
                    CheckpointTick::Skipped
                }
                Err(panicked) => {
                    // The cycle panicked: its connection is gone (reopened next tick) and the
                    // escalation state is the one it was handed, counted as a TRUNCATE attempt.
                    truncate_state = panicked.truncate_state;
                    tracing::warn!(
                        error = %panicked.join_error,
                        "WAL checkpoint cycle panicked on its blocking thread; \
                         dropping the connection for a fresh reopen next tick"
                    );
                    note_checkpoint_skipped();
                    CheckpointTick::Skipped
                }
            }
        };

        // A no-progress or unmeasured TRUNCATE returns a bounded attribution
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
        // connection was unavailable or its PASSIVE result was busy, not that
        // some registered span held the pool's writer mutex (a checkpoint
        // tick no longer touches it at all) — but the sweep must not go blind for the
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
            log_wal_high_water_warn(
                wal_pages,
                config.high_water_pages,
                oldest_tx.as_ref(),
                config.tx_warn_secs,
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
    log_tx_registry_entries_warn(wal_pages, &khive_storage::tx_registry::snapshot());
}

fn log_tx_registry_entries_warn(wal_pages: u64, snapshot: &[(Duration, Option<String>)]) {
    for (age, label) in snapshot {
        tracing::warn!(
            wal_pages,
            tx_age_secs = age.as_secs_f64(),
            tx_label = label.as_deref().unwrap_or("<unlabeled>"),
            "WAL high-water: open transaction registry entry"
        );
    }
}

fn log_truncate_no_progress_warn(
    wal_pages_before: u64,
    wal_pages_after: u64,
    snapshot: &[(Duration, Option<String>)],
) {
    let open_tx_count = snapshot.len();
    let oldest_tx_age_secs = snapshot
        .iter()
        .map(|(age, _)| *age)
        .max()
        .map(|age| age.as_secs_f64());
    if snapshot.is_empty() {
        tracing::warn!(
            wal_pages_before,
            wal_pages_after,
            open_tx_count,
            oldest_tx_age_secs = ?oldest_tx_age_secs,
            "WAL TRUNCATE attempt made no progress; no open transaction in this process's registry"
        );
    } else {
        tracing::warn!(
            wal_pages_before,
            wal_pages_after,
            open_tx_count,
            oldest_tx_age_secs = ?oldest_tx_age_secs,
            "WAL TRUNCATE attempt made no progress; open transactions observed in this process's registry"
        );
    }
    log_tx_registry_entries_warn(wal_pages_after, snapshot);
}

/// Emits the high-water WARN, deciding its text from the registry entry this
/// tick already read instead of asserting a cause the evidence beside it can
/// refute.
///
/// The registry only covers this process. An old registered transaction may
/// hold a snapshot; a young or empty registry cannot rule out an external
/// reader. `warn_after` is the same threshold as the transaction-age ladder.
fn log_wal_high_water_warn(
    wal_pages: u64,
    high_water: u64,
    oldest: Option<&khive_storage::tx_registry::OldestSpan>,
    warn_after: Duration,
) {
    match oldest.filter(|span| span.age >= warn_after) {
        Some(span) => tracing::warn!(
            wal_pages,
            high_water,
            oldest_tx_age_secs = span.age.as_secs_f64(),
            oldest_tx_label = span.label.as_deref().unwrap_or("<unlabeled>"),
            "WAL high-water mark exceeded; an in-process registered transaction is older \
             than the age threshold and may hold a snapshot"
        ),
        None => tracing::warn!(
            wal_pages,
            high_water,
            oldest_tx_age_secs = ?oldest.map(|span| span.age.as_secs_f64()),
            oldest_tx_label = oldest
                .and_then(|span| span.label.as_deref())
                .unwrap_or("<none>"),
            "WAL high-water mark exceeded; no in-process transaction older than the age \
             threshold is visible; a reader in another process may hold the snapshot, \
             or writes may outpace PASSIVE checkpoints"
        ),
    }
}

/// Internal result of the synchronous SQLite checkpoint core. Keeping the
/// no-progress attribution request next to (but distinct from) `wal_pages`
/// makes the async handoff explicit and gives deferred work one caller-owned
/// lifetime instead of leaving it in mutable cross-tick state.
#[derive(Debug)]
#[must_use]
struct CheckpointCoreOutcome {
    wal_pages: Option<u64>,
    unavailable_reason: Option<CheckpointUnavailableReason>,
    sidecar_attribution: Option<WalpinAttributionRequest>,
}

/// Issue one checkpoint cycle against the task's dedicated checkpoint
/// connection (`conn` — see `CheckpointConnection`; NEVER the pool's writer
/// mutex).
///
/// Returns the observed WAL page count on success. A busy PASSIVE row has no
/// usable observation and the compatibility wrapper returns `SQLITE_BUSY`;
/// an inconsistent nonbusy frame pair instead returns `SQLITE_ERROR`.
/// A connection-level pragma error is also returned; the task caller drops
/// that connection and reopens next tick. TRUNCATE errors remain non-fatal.
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
    let outcome = checkpoint_once_core(pool, conn, config, truncate_state)?;
    outcome.wal_pages.ok_or_else(|| match outcome
        .unavailable_reason
        .expect("unavailable core outcome has a reason")
    {
        CheckpointUnavailableReason::Busy => rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("PASSIVE checkpoint returned a busy row without a WAL frame observation".into()),
        ),
        CheckpointUnavailableReason::InconsistentFrames => rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some("PASSIVE checkpoint returned an inconsistent frame pair without a WAL frame observation".into()),
        ),
    })
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
    let started = Instant::now();
    let checkpoint_result = query_routine_checkpoint_observation(pool, conn);
    let elapsed_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    record_checkpoint_timing(
        pool,
        elapsed_us,
        checkpoint_result
            .as_ref()
            .ok()
            .map(|observation| observation.busy),
    );
    let raw_observation = match checkpoint_result {
        Ok(observation) => observation,
        Err(e) => {
            record_checkpoint_run_result(pool, None);
            tracing::warn!(error = %e, elapsed_us, "WAL checkpoint failed");
            return Err(e);
        }
    };
    record_checkpoint_run_result(
        pool,
        Some((
            raw_observation.busy,
            raw_observation.log_frames,
            raw_observation.checkpointed_frames,
        )),
    );
    let wal_pages = match observed_wal_pages(raw_observation) {
        Ok(wal_pages) => wal_pages,
        Err(reason) => {
            match reason {
                CheckpointUnavailableReason::Busy => tracing::debug!(
                    busy = raw_observation.busy,
                    wal_log_frames = raw_observation.log_frames,
                    wal_checkpointed_frames = raw_observation.checkpointed_frames,
                    elapsed_us,
                    "WAL PASSIVE checkpoint returned a busy row; frame observation unavailable"
                ),
                CheckpointUnavailableReason::InconsistentFrames => tracing::warn!(
                    busy = raw_observation.busy,
                    wal_log_frames = raw_observation.log_frames,
                    wal_checkpointed_frames = raw_observation.checkpointed_frames,
                    elapsed_us,
                    "WAL PASSIVE checkpoint returned an inconsistent frame pair; frame observation unavailable"
                ),
            }
            return Ok(CheckpointCoreOutcome {
                wal_pages: None,
                unavailable_reason: Some(reason),
                sidecar_attribution: None,
            });
        }
    };
    let observation = record_routine_wal_observation(pool, raw_observation);
    LAST_WAL_PAGES.store(wal_pages, Ordering::Relaxed);
    note_checkpoint_observed(wal_pages);
    tracing::debug!(
        wal_pages,
        elapsed_us,
        busy = raw_observation.busy,
        wal_checkpointed_frames = observation.checkpointed_frames,
        wal_pending_frames = observation.pending_frames,
        wal_physical_bytes = ?observation.physical_wal_bytes,
        "WAL checkpoint issued"
    );

    let sidecar_attribution = maybe_truncate(pool, conn, config, wal_pages, truncate_state);

    Ok(CheckpointCoreOutcome {
        wal_pages: Some(wal_pages),
        unavailable_reason: None,
        sidecar_attribution,
    })
}

fn truncate_needs_attribution(wal_pages_before: u64, wal_pages_after: Option<u64>) -> bool {
    wal_pages_after.is_none_or(|pages| pages >= wal_pages_before)
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
    #[cfg(test)]
    off_worker::cycle_panic_seam::after_attempt_decided(pool.canonical_path());

    let start = Instant::now();
    let outcome = query_truncate_observation(conn);
    record_checkpoint_run_result(
        pool,
        outcome.as_ref().ok().map(|observation| {
            (
                observation.busy,
                observation.log_frames,
                observation.checkpointed_frames,
            )
        }),
    );
    let elapsed = start.elapsed();

    // Restore the pool's configured busy_timeout immediately after the
    // attempt, win or lose, before any other logging or bookkeeping.
    if let Err(e) = conn.busy_timeout(original_busy_timeout) {
        tracing::warn!(error = %e, "failed to restore busy_timeout after TRUNCATE attempt");
    }

    match outcome {
        Ok(_) => {
            let wal_pages_after = query_wal_pages(pool, conn);
            if let Some(pages) = wal_pages_after {
                tracing::info!(
                    wal_pages_before,
                    wal_pages_after = pages,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "WAL TRUNCATE checkpoint attempted"
                );
            } else {
                tracing::info!(
                    wal_pages_before,
                    wal_pages_after_unavailable = true,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "WAL TRUNCATE checkpoint attempted"
                );
            }

            if truncate_needs_attribution(wal_pages_before, wal_pages_after) {
                let snapshot = khive_storage::tx_registry::snapshot();
                if let Some(pages) = wal_pages_after {
                    log_truncate_no_progress_warn(wal_pages_before, pages, &snapshot);
                } else {
                    tracing::warn!(
                        wal_pages_before,
                        "WAL TRUNCATE progress unmeasured; checking possible holders in this process and others"
                    );
                    log_tx_registry_entries_warn(wal_pages_before, &snapshot);
                }
                #[cfg(test)]
                if let Some(path) = pool.canonical_path() {
                    truncate_report_test_sync::after_no_progress_before_report(path);
                }
                #[cfg(unix)]
                {
                    // The census above had to be captured before TRUNCATE so
                    // a transient holder remains attributable. The bounded
                    // sidecar walk itself must not run here: it keeps its own
                    // awaited pass in the async owner, which orders it against
                    // the tick's housekeeping. Hand the immutable request back
                    // to that owner for its `spawn_blocking` pass.
                    sidecar_attribution = holder_attribution.take();
                }
                log_backfill_gap(pool, conn);
            }

            note_truncate_outcome(config, wal_pages_after, truncate_state);
        }
        Err(e) => {
            tracing::warn!(error = %e, wal_pages_before, "WAL TRUNCATE attempt failed");
            log_tx_registry_snapshot_warn(wal_pages_before);
            note_truncate_outcome(config, Some(wal_pages_before), truncate_state);
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

/// ADR-091 Plank 2: track measured TRUNCATE outcomes that fail to bring
/// `wal_pages` below `warn_pages`, firing a one-shot escalated WARN at the
/// third such failure. An unmeasured attempt leaves the streak unchanged;
/// only a measured result below `warn_pages` resets it.
fn note_truncate_outcome(
    config: &CheckpointConfig,
    wal_pages_after: Option<u64>,
    state: &mut TruncateState,
) {
    // Metrics read-surface (load/perf harness): this function runs exactly
    // once per genuine TRUNCATE attempt (both the `Ok` and `Err` outcome
    // arms in `maybe_truncate` call it once each), so incrementing here
    // counts total attempts without a separate call site.
    TRUNCATE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);

    if let Some(wal_pages_after) = wal_pages_after {
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
/// report the one-row backfill gap as `log` minus `checkpointed` from its
/// 3-column return row when that row is informative. A busy or malformed row
/// reports the gap as unavailable, never as zero. A gap alone does not
/// establish a reader pin. Zero
/// dependence on SQLite's shm WAL-index layout (ADR-091 Amendment 22).
fn log_backfill_gap(pool: &ConnectionPool, conn: &rusqlite::Connection) {
    match query_backfill_gap(conn) {
        Ok(observation) => {
            record_checkpoint_run_result(
                pool,
                Some((
                    observation.busy,
                    observation.log_frames,
                    observation.checkpointed_frames,
                )),
            );
            if observed_wal_pages(observation).is_ok() {
                tracing::warn!(
                    busy = observation.busy,
                    wal_log_frames = observation.log_frames,
                    wal_checkpointed_frames = observation.checkpointed_frames,
                    backfill_gap_frames = observation
                        .log_frames
                        .saturating_sub(observation.checkpointed_frames)
                        .max(0),
                    "ADR-091 Plank C: WAL backfill gap after TRUNCATE no-progress"
                );
            } else {
                tracing::warn!(
                    busy = observation.busy,
                    wal_log_frames = observation.log_frames,
                    wal_checkpointed_frames = observation.checkpointed_frames,
                    "ADR-091 Plank C: WAL backfill gap unavailable after TRUNCATE"
                );
            }
        }
        Err(e) => {
            record_checkpoint_run_result(pool, None);
            tracing::warn!(
                error = %e,
                "ADR-091 Plank C: failed to query WAL backfill gap"
            );
        }
    }
}

/// ADR-091 Amendment 2 Plank C: issue `PRAGMA wal_checkpoint(PASSIVE)` and
/// return its `(log, checkpointed)` columns (index 1 and 2 of the 3-column
/// return row). PASSIVE never blocks readers or writers. The backfill gap is
/// `log - checkpointed`; extracted as its own pure query so the arithmetic is
/// unit-testable against a real SQLite connection without depending on
/// `tracing` capture.
fn query_backfill_gap(conn: &rusqlite::Connection) -> rusqlite::Result<RawCheckpointObservation> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok(RawCheckpointObservation {
            busy: row.get(0)?,
            log_frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    })
}

/// Evaluate whether a threshold-crossing WARN should fire and advance the
/// crossing-state flag.
///
/// Returns `true` on a false→true transition in `now_above` (first observed
/// above-threshold tick after a below-threshold tick), `false` on any other
/// tick. The `was_above` flag is updated in-place to track state across calls.
/// `run_checkpoint_task` uses this for the `high_water_pages` threshold;
/// `observe_wal_pages` owns the separate `warn_pages` severity ladder.
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

/// Why a syntactically valid SQLite checkpoint row has no usable frame count.
/// Keep the raw `busy` indication distinct from an inconsistent frame pair:
/// only SQLite's nonzero busy column may become a busy error or busy log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointUnavailableReason {
    Busy,
    InconsistentFrames,
}

fn observed_wal_pages(
    observation: RawCheckpointObservation,
) -> Result<u64, CheckpointUnavailableReason> {
    if observation.busy != 0 {
        return Err(CheckpointUnavailableReason::Busy);
    }
    if observation.log_frames == -1 && observation.checkpointed_frames == -1 {
        // SQLite reports an absent WAL with two -1 frame columns.
        return Ok(0);
    }
    if observation.log_frames >= 0
        && observation.checkpointed_frames >= 0
        && observation.checkpointed_frames <= observation.log_frames
    {
        Ok(observation.log_frames as u64)
    } else {
        Err(CheckpointUnavailableReason::InconsistentFrames)
    }
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

/// The routine caller uses the real SQLite row. Unit tests can inject one
/// exact raw row for a uniquely keyed pool to exercise a frame combination
/// that SQLite does not normally emit without changing the production path.
fn query_routine_checkpoint_observation(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
) -> rusqlite::Result<RawCheckpointObservation> {
    #[cfg(test)]
    if let Some(row) = test_take_passive_row(pool) {
        return Ok(row);
    }
    #[cfg(not(test))]
    let _ = pool;
    query_checkpoint_observation(conn)
}

#[cfg(test)]
static TEST_PASSIVE_ROWS: OnceLock<Mutex<HashMap<Option<PathBuf>, RawCheckpointObservation>>> =
    OnceLock::new();

#[cfg(test)]
fn test_passive_rows() -> &'static Mutex<HashMap<Option<PathBuf>, RawCheckpointObservation>> {
    TEST_PASSIVE_ROWS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn test_arm_passive_row(pool: &ConnectionPool, row: RawCheckpointObservation) {
    test_passive_rows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(checkpoint_db_key(pool), row);
}

#[cfg(test)]
fn test_take_passive_row(pool: &ConnectionPool) -> Option<RawCheckpointObservation> {
    test_passive_rows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&checkpoint_db_key(pool))
}

fn query_truncate_observation(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<RawCheckpointObservation> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
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
fn query_wal_pages(pool: &ConnectionPool, conn: &rusqlite::Connection) -> Option<u64> {
    let observation = query_checkpoint_observation(conn);
    record_checkpoint_run_result(
        pool,
        observation.as_ref().ok().map(|observation| {
            (
                observation.busy,
                observation.log_frames,
                observation.checkpointed_frames,
            )
        }),
    );
    let pages = observation
        .ok()
        .and_then(|row| observed_wal_pages(row).ok());
    if let Some(pages) = pages {
        LAST_WAL_PAGES.store(pages, Ordering::Relaxed);
        note_checkpoint_observed(pages);
    }
    pages
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
