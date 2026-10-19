//! Synchronous checkpoint execution and TRUNCATE escalation.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::pool::ConnectionPool;

#[cfg(unix)]
use super::capture_walpin_attribution_request;
#[cfg(test)]
use super::off_worker;
#[cfg(doc)]
use super::run_checkpoint_task;
use super::{
    log_backfill_gap, note_checkpoint_observed, observed_wal_pages,
    query_routine_checkpoint_observation, query_truncate_observation, query_wal_pages,
    record_checkpoint_run_result, record_checkpoint_timing, record_routine_wal_observation,
    CheckpointConfig, CheckpointUnavailableReason, TruncateState, WalpinAttributionRequest,
    LAST_WAL_PAGES, TRUNCATE_ATTEMPTS, TRUNCATE_CONSECUTIVE_FAILURES,
};

/// ADR-091 Plank 0 (Amendment 3: takes the tick's already-computed,
/// backend-scoped oldest span instead of re-querying the process-wide
/// aggregate): log the oldest open transaction registry entry alongside the
/// WAL frame count at `debug!`, on EVERY tick regardless of threshold
/// state. This is the low-volume per-tick trace; the WARN-level escalations
/// live in [`log_tx_registry_oldest_warn`] and
/// debug-level, unconditional per-tick trace. See
/// crates/khive-db/docs/api/checkpoint.md#private-tx-registry-logging-helpers-plank-0
pub(super) fn log_tx_registry_oldest_debug(
    wal_pages: u64,
    oldest: Option<&khive_storage::tx_registry::OldestSpan>,
) {
    if let Some(span) = oldest {
        tracing::debug!(target: "khive_db::checkpoint",
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
pub(super) fn log_tx_registry_oldest_warn(
    wal_pages: u64,
    oldest: Option<&khive_storage::tx_registry::OldestSpan>,
) {
    if let Some(span) = oldest {
        tracing::warn!(target: "khive_db::checkpoint",
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
pub(super) fn log_tx_registry_snapshot_warn(wal_pages: u64) {
    log_tx_registry_entries_warn(wal_pages, &khive_storage::tx_registry::snapshot());
}

fn log_tx_registry_entries_warn(wal_pages: u64, snapshot: &[(Duration, Option<String>)]) {
    for (age, label) in snapshot {
        tracing::warn!(target: "khive_db::checkpoint",
            wal_pages,
            tx_age_secs = age.as_secs_f64(),
            tx_label = label.as_deref().unwrap_or("<unlabeled>"),
            "WAL high-water: open transaction registry entry"
        );
    }
}

pub(super) fn log_truncate_no_progress_warn(
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
        tracing::warn!(target: "khive_db::checkpoint",
            wal_pages_before,
            wal_pages_after,
            open_tx_count,
            oldest_tx_age_secs = ?oldest_tx_age_secs,
            "WAL TRUNCATE attempt made no progress; no open transaction in this process's registry"
        );
    } else {
        tracing::warn!(target: "khive_db::checkpoint",
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
pub(super) fn log_wal_high_water_warn(
    wal_pages: u64,
    high_water: u64,
    oldest: Option<&khive_storage::tx_registry::OldestSpan>,
    warn_after: Duration,
) {
    match oldest.filter(|span| span.age >= warn_after) {
        Some(span) => tracing::warn!(target: "khive_db::checkpoint",
            wal_pages,
            high_water,
            oldest_tx_age_secs = span.age.as_secs_f64(),
            oldest_tx_label = span.label.as_deref().unwrap_or("<unlabeled>"),
            "WAL high-water mark exceeded; an in-process registered transaction is older \
             than the age threshold and may hold a snapshot"
        ),
        None => tracing::warn!(target: "khive_db::checkpoint",
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
pub(crate) struct CheckpointCoreOutcome {
    pub(super) wal_pages: Option<u64>,
    pub(super) unavailable_reason: Option<CheckpointUnavailableReason>,
    pub(super) sidecar_attribution: Option<WalpinAttributionRequest>,
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
pub(crate) fn checkpoint_once_core(
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
            tracing::warn!(
                target: "khive_db::checkpoint",
                error = %e,
                elapsed_us,
                "WAL checkpoint failed"
            );
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
                CheckpointUnavailableReason::Busy => {
                    tracing::debug!(target: "khive_db::checkpoint",
                        busy = raw_observation.busy,
                        wal_log_frames = raw_observation.log_frames,
                        wal_checkpointed_frames = raw_observation.checkpointed_frames,
                        elapsed_us,
                        "WAL PASSIVE checkpoint returned a busy row; frame observation unavailable"
                    )
                }
                CheckpointUnavailableReason::InconsistentFrames => {
                    tracing::warn!(target: "khive_db::checkpoint",
                        busy = raw_observation.busy,
                        wal_log_frames = raw_observation.log_frames,
                        wal_checkpointed_frames = raw_observation.checkpointed_frames,
                        elapsed_us,
                        "WAL PASSIVE checkpoint returned an inconsistent frame pair; frame observation unavailable"
                    )
                }
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
    tracing::debug!(target: "khive_db::checkpoint",
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

pub(super) fn truncate_needs_attribution(
    wal_pages_before: u64,
    wal_pages_after: Option<u64>,
) -> bool {
    wal_pages_after.is_none_or(|pages| pages >= wal_pages_before)
}

/// Evaluate and, if due, attempt a TRUNCATE escalation on the same dedicated
/// checkpoint connection the caller already holds (never its own checkout —
/// there is no pool writer involved on this path at all). `last_attempt`
/// is stamped ONLY on an actual attempt, never on a skip. See
/// crates/khive-db/docs/api/checkpoint.md#maybe_truncate--truncate-attempt-gating-plank-2
pub(super) fn maybe_truncate(
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
        tracing::warn!(
            target: "khive_db::checkpoint",
            error = %e,
            "failed to lower busy_timeout for TRUNCATE attempt; skipping"
        );
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
        tracing::warn!(
            target: "khive_db::checkpoint",
            error = %e,
            "failed to restore busy_timeout after TRUNCATE attempt"
        );
    }

    match outcome {
        Ok(_) => {
            let wal_pages_after = query_wal_pages(pool, conn);
            if let Some(pages) = wal_pages_after {
                tracing::info!(target: "khive_db::checkpoint",
                    wal_pages_before,
                    wal_pages_after = pages,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "WAL TRUNCATE checkpoint attempted"
                );
            } else {
                tracing::info!(target: "khive_db::checkpoint",
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
                    tracing::warn!(target: "khive_db::checkpoint",
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
            tracing::warn!(
                target: "khive_db::checkpoint",
                error = %e,
                wal_pages_before,
                "WAL TRUNCATE attempt failed"
            );
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

/// ADR-091 Plank 2: track measured TRUNCATE outcomes that fail to bring
/// `wal_pages` below `warn_pages`, firing a one-shot escalated WARN at the
/// third such failure. An unmeasured attempt leaves the streak unchanged;
/// only a measured result below `warn_pages` resets it.
pub(super) fn note_truncate_outcome(
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
                tracing::warn!(target: "khive_db::checkpoint",
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

#[cfg(test)]
pub(super) mod truncate_report_test_sync {
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
