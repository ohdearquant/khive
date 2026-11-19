//! Checkpoint result-row queries and frame-observation validation.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use crate::pool::ConnectionPool;

#[cfg(test)]
use super::checkpoint_db_key;
use super::{note_checkpoint_observed, record_checkpoint_run_result, LAST_WAL_PAGES};

/// ADR-091 Amendment 2 Plank C: on a TRUNCATE no-progress event, run a fresh
/// `PRAGMA wal_checkpoint(PASSIVE)` (never blocks readers or writers) and
/// report the one-row backfill gap as `log` minus `checkpointed` from its
/// 3-column return row when that row is informative. A busy or malformed row
/// reports the gap as unavailable, never as zero. A gap alone does not
/// establish a reader pin. Zero
/// dependence on SQLite's shm WAL-index layout (ADR-091 Amendment 22).
pub(super) fn log_backfill_gap(pool: &ConnectionPool, conn: &rusqlite::Connection) {
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
                tracing::warn!(target: "khive_db::checkpoint",
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
                tracing::warn!(target: "khive_db::checkpoint",
                    busy = observation.busy,
                    wal_log_frames = observation.log_frames,
                    wal_checkpointed_frames = observation.checkpointed_frames,
                    "ADR-091 Plank C: WAL backfill gap unavailable after TRUNCATE"
                );
            }
        }
        Err(e) => {
            record_checkpoint_run_result(pool, None);
            tracing::warn!(target: "khive_db::checkpoint",
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
pub(super) fn query_backfill_gap(
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

/// Evaluate whether a threshold-crossing WARN should fire and advance the
/// crossing-state flag.
///
/// Returns `true` on a false→true transition in `now_above` (first observed
/// above-threshold tick after a below-threshold tick), `false` on any other
/// tick. The `was_above` flag is updated in-place to track state across calls.
/// `run_checkpoint_task` uses this for the `high_water_pages` threshold;
/// `observe_wal_pages` owns the separate `warn_pages` severity ladder.
pub(super) fn crossing_warn(now_above: bool, was_above: &mut bool) -> bool {
    let fire = now_above && !*was_above;
    *was_above = now_above;
    fire
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RawCheckpointObservation {
    pub(super) busy: i64,
    pub(super) log_frames: i64,
    pub(super) checkpointed_frames: i64,
}

/// Why a syntactically valid SQLite checkpoint row has no usable frame count.
/// Keep the raw `busy` indication distinct from an inconsistent frame pair:
/// only SQLite's nonzero busy column may become a busy error or busy log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckpointUnavailableReason {
    Busy,
    InconsistentFrames,
}

pub(super) fn observed_wal_pages(
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
pub(super) fn query_checkpoint_observation(
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
pub(super) fn query_routine_checkpoint_observation(
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
pub(super) fn test_arm_passive_row(pool: &ConnectionPool, row: RawCheckpointObservation) {
    test_passive_rows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(checkpoint_db_key(pool), row);
}

#[cfg(test)]
pub(super) fn test_take_passive_row(pool: &ConnectionPool) -> Option<RawCheckpointObservation> {
    test_passive_rows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&checkpoint_db_key(pool))
}

pub(super) fn query_truncate_observation(
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
pub(super) fn query_wal_pages(pool: &ConnectionPool, conn: &rusqlite::Connection) -> Option<u64> {
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
