use super::{ConnectionPool, StorageError, WriterAcquisitionCounters, WriterAcquisitionSnapshot};
use std::sync::atomic::Ordering;

impl WriterAcquisitionCounters {
    pub(crate) fn record_writer_task_acquisition(&self) {
        self.writer_task_acquisitions
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records one writer-task `BEGIN IMMEDIATE` refused busy or locked.
    /// Called for every such refusal, whether or not a bounded retry goes
    /// on to absorb it — this is the caller-facing contention count, and it
    /// alone must equal the number of busy/locked refusals SQLite actually
    /// returned, independent of retry policy.
    pub(crate) fn record_writer_task_begin_busy(&self) {
        self.writer_task_begin_busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one busy or locked `BEGIN IMMEDIATE` refusal hidden from the
    /// caller by a subsequent bounded retry. This counter moves before the
    /// next BEGIN attempt, in addition to (never instead of) the
    /// `writer_task_begin_busy` call for the same refusal; it never implies
    /// that the request closure ran.
    pub(crate) fn record_writer_task_begin_busy_absorbed(&self) {
        self.writer_task_begin_busy_absorbed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records one writer-task `BEGIN IMMEDIATE` that failed for any other
    /// reason. Without this the non-busy arm reproduces, one level down, the
    /// same silent-failure gap the busy counter closes.
    pub(crate) fn record_writer_task_begin_error(&self) {
        self.writer_task_begin_errors
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records one dequeued writer-task request that reached the writer seam
    /// and terminated in error. Called exactly once per such request,
    /// regardless of which terminal state it produced.
    pub(crate) fn record_writer_task_request_failure(&self) {
        self.writer_task_request_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records the subset of [`Self::record_writer_task_request_failure`]
    /// whose terminal state was `SideEffectsUnknown`. Callers pair this call
    /// with a `record_writer_task_request_failure()` call for the same
    /// request rather than in place of it.
    pub(crate) fn record_writer_task_side_effects_unknown(&self) {
        self.writer_task_side_effects_unknown
            .fetch_add(1, Ordering::Relaxed);
    }

    /// `lease_timeouts` is owned by the pool's write admission, which takes
    /// the volume lease for every writer class, so the pool supplies it.
    pub(super) fn snapshot(&self, lease_timeouts: u64) -> WriterAcquisitionSnapshot {
        let pooled_acquisitions = self.pooled_acquisitions.load(Ordering::Relaxed);
        let standalone_acquisitions = self.standalone_acquisitions.load(Ordering::Relaxed);
        let writer_task_acquisitions = self.writer_task_acquisitions.load(Ordering::Relaxed);
        WriterAcquisitionSnapshot {
            acquisitions: pooled_acquisitions
                .saturating_add(standalone_acquisitions)
                .saturating_add(writer_task_acquisitions),
            pooled_acquisitions,
            standalone_acquisitions,
            writer_task_acquisitions,
            timeouts: self.pooled_timeouts.load(Ordering::Relaxed),
            lease_timeouts,
            direct_busy_refusals: self.direct_busy_refusals.load(Ordering::Relaxed),
            writer_task_begin_busy: self.writer_task_begin_busy.load(Ordering::Relaxed),
            writer_task_begin_busy_absorbed: self
                .writer_task_begin_busy_absorbed
                .load(Ordering::Relaxed),
            writer_task_begin_errors: self.writer_task_begin_errors.load(Ordering::Relaxed),
            writer_task_request_failures: self.writer_task_request_failures.load(Ordering::Relaxed),
            writer_task_side_effects_unknown: self
                .writer_task_side_effects_unknown
                .load(Ordering::Relaxed),
            writer_guard_drop_rollbacks: self.writer_guard_drop_rollbacks.load(Ordering::Relaxed),
        }
    }
}

impl ConnectionPool {
    /// Observe one final direct execution error without changing its classification.
    /// Checkout, connection-open, reader and writer-task errors do not call this boundary.
    pub(crate) fn record_direct_writer_error(&self, error: &StorageError) {
        if direct_writer_sqlite_code(error) == Some(rusqlite::ErrorCode::DatabaseBusy) {
            self.writer_acquisition_counters
                .direct_busy_refusals
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a pooled writer guard dropped inside a transaction: count it in
    /// [`WriterAcquisitionSnapshot::writer_guard_drop_rollbacks`] and write a
    /// `writer_guard_drop` sink row naming how the drop settled it.
    pub(crate) fn record_writer_guard_drop(
        &self,
        settlement: &Result<(), crate::error::SqliteError>,
    ) {
        self.writer_acquisition_counters
            .writer_guard_drop_rollbacks
            .fetch_add(1, Ordering::Relaxed);
        let outcome = match settlement {
            Ok(()) => "guard dropped with open transaction; rolled back".to_string(),
            Err(error) => {
                format!("guard dropped with open transaction; writer retired: {error}")
            }
        };
        let db = crate::timeout_sink::db_label(self);
        tracing::warn!(db = %db, %outcome, "pooled writer guard settled on drop");
        crate::timeout_sink::emit_writer_guard_drop(&db, &outcome);
    }
}

// Public atomic callbacks can supply cyclic Error source chains.
// A finite walk bounds observation when each source() call returns.
const DIRECT_WRITER_SOURCE_LIMIT: usize = 32;

/// Inspect at most 32 nodes, counting the returned StorageError as node one,
/// and make at most 32 source() calls. Preserved storage wrappers are included;
/// message text, deeper causes and cause-free outcomes cannot supply a code.
fn direct_writer_sqlite_code(error: &StorageError) -> Option<rusqlite::ErrorCode> {
    let mut cause: &(dyn std::error::Error + 'static) = error;
    for _ in 0..DIRECT_WRITER_SOURCE_LIMIT {
        if let Some(sqlite) = cause.downcast_ref::<rusqlite::Error>() {
            return sqlite.sqlite_error_code();
        }
        if let Some(crate::error::SqliteError::Rusqlite(sqlite)) =
            cause.downcast_ref::<crate::error::SqliteError>()
        {
            return sqlite.sqlite_error_code();
        }
        cause = cause.source()?;
    }
    None
}
