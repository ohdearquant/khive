//! Checkpoint run state, routine observations, timing, and metric accessors.

use super::{
    BTreeMap, ConnectionPool, Duration, HashMap, Instant, Mutex, OnceLock, Ordering, Path, PathBuf,
    RawCheckpointObservation, CHECKPOINT_CONSECUTIVE_SKIPS, CHECKPOINT_LAST_SKIP_WAL_PAGES,
    CHECKPOINT_LIFECYCLE_APPEND_ATTEMPTS, CHECKPOINT_LIFECYCLE_APPEND_FAILURES,
    CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS, CHECKPOINT_PRESSURE_ELEVATED_TICKS,
    CHECKPOINT_PRESSURE_EPISODES_RECOVERED, CHECKPOINT_PRESSURE_EPISODES_STARTED,
    CHECKPOINT_SKIPPED_TICKS, LAST_WAL_PAGES, READ_TX_MAX_AGE_EVICTIONS, TRUNCATE_ATTEMPTS,
    TRUNCATE_CONSECUTIVE_FAILURES,
};

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
pub(super) struct CheckpointRunEntry {
    pub(super) run: CheckpointRun,
    first_observed_at: Instant,
    pub(super) last_log_frames: i64,
    last_informative_at: Instant,
    pub(super) busy_since_last_informative: bool,
}

#[derive(Debug, Default)]
pub(super) struct CheckpointRunState {
    active_tasks: usize,
    pub(super) checkpoint_interval_ms: u64,
    owner_intervals_ms: BTreeMap<u64, usize>,
    pub(super) entry: Option<CheckpointRunEntry>,
}

static CHECKPOINT_RUNS: OnceLock<Mutex<HashMap<Option<PathBuf>, CheckpointRunState>>> =
    OnceLock::new();

pub(super) fn checkpoint_runs() -> &'static Mutex<HashMap<Option<PathBuf>, CheckpointRunState>> {
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

pub(super) fn advance_checkpoint_run_at(
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
pub(super) fn advance_checkpoint_run(
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

pub(super) fn checkpoint_timings() -> &'static Mutex<HashMap<Option<PathBuf>, CheckpointTiming>> {
    CHECKPOINT_TIMINGS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn record_checkpoint_timing(pool: &ConnectionPool, elapsed_us: u64, busy: Option<i64>) {
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

pub(super) fn checkpoint_db_key_from_path(path: Option<&Path>) -> Option<PathBuf> {
    path.map(Path::to_path_buf)
}

pub(super) fn checkpoint_db_key(pool: &ConnectionPool) -> Option<PathBuf> {
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

pub(super) fn record_routine_wal_observation(
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
pub(super) fn note_checkpoint_skipped() {
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
pub(super) fn note_checkpoint_observed(_wal_pages: u64) {
    CHECKPOINT_CONSECUTIVE_SKIPS.store(0, Ordering::Relaxed);
}

pub(super) fn note_checkpoint_pressure_observation(above_warn: bool, was_above_warn: bool) {
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
