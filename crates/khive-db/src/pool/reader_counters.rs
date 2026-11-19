//! Reader acquisition, timeout, and completed-hold accounting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[cfg(doc)]
use super::{ConnectionPool, ReaderGuard, WriterAcquisitionSnapshot};

/// Why a caller is allowed to bypass the pooled reader queue.
///
/// This is deliberately a closed, crate-private list. Ordinary request reads
/// have no variant: they must use [`ConnectionPool::reader`] and surface its
/// bounded admission timeout without falling back to a fresh connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // The closed ADR list includes infrastructure paths not instantiated today.
pub(crate) enum StandaloneReaderPurpose {
    /// ADR-005/ADR-091's explicit, multi-call deferred raw-SQL transaction.
    ExplicitSqlReadTransaction,
    /// Boot-time schema/model-registry inspection before a runtime pool can
    /// own the read. Kept separate from request traffic in diagnostics.
    BootSchemaProbe,
    /// An operator diagnostic that requires a physically independent
    /// snapshot. Kept separate from request traffic in diagnostics.
    DiagnosticsIndependentSnapshot,
}

impl StandaloneReaderPurpose {
    fn is_infrastructure(self) -> bool {
        matches!(
            self,
            Self::BootSchemaProbe | Self::DiagnosticsIndependentSnapshot
        )
    }
}

/// Process-local reader acquisition and hold lifecycle since one
/// [`ConnectionPool`] was constructed.
///
/// Counters are monotonic and reset only when the pool is reconstructed.
/// `active_pooled_checkouts` is the point-in-time number of live
/// [`ReaderGuard`] values. Hold duration includes connection reset or
/// replacement on return, because the slot is not reusable before then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderAcquisitionSnapshot {
    /// Configured process-local admission budget shared by pooled readers and
    /// the explicit raw-SQL read-transaction exception.
    pub reader_admission_capacity: usize,
    /// Point-in-time permits not held by either pooled readers or explicit
    /// raw-SQL read transactions.
    pub available_reader_admission_slots: usize,
    /// Successful request-path reader acquisitions (pooled plus the explicit
    /// raw-SQL transaction exception; infrastructure opens excluded).
    pub acquisitions: u64,
    /// Successful bounded pooled-reader checkouts.
    pub pooled_checkouts: u64,
    /// Successful request-path standalone opens from the closed exception
    /// list (currently explicit raw-SQL deferred transactions only).
    pub standalone_opens: u64,
    /// Successful boot/diagnostic standalone opens, attributed separately so
    /// request traffic cannot be inferred from infrastructure activity.
    pub infrastructure_standalone_opens: u64,
    /// Reader-admission waits that exhausted `checkout_timeout` before any
    /// query began. Covers pooled checkout and the closed raw-SQL exception;
    /// cooperative request cancellation is intentionally excluded.
    pub checkout_timeouts: u64,
    /// Queries on a checked-out pooled reader that SQLite refused with
    /// `SQLITE_BUSY` after the connection's busy handler gave up (typed-store
    /// reads, pooled raw-SQL reads, and [`ReaderGuard::query_row`]). Counted
    /// after checkout succeeded, so it never overlaps `checkout_timeouts`;
    /// `SQLITE_LOCKED` and cooperative cancellation are excluded. Writer
    /// refusals are not counted here; the writer task's are in
    /// [`WriterAcquisitionSnapshot::writer_task_begin_busy`].
    pub busy_timeouts: u64,
    /// Pooled checkouts live at the instant this snapshot was taken.
    pub active_pooled_checkouts: u64,
    /// High-water mark of concurrent pooled checkouts.
    pub peak_active_pooled_checkouts: u64,
    /// Pooled guards that completed their full return/reset lifecycle.
    pub completed_pooled_checkouts: u64,
    /// Longest completed checkout hold, including return/reset, in
    /// microseconds. Diagnostic evidence only; never a test timing gate.
    pub max_completed_hold_micros: u64,
    /// The typed-store operation that held the checkout reported in
    /// `max_completed_hold_micros`. `None` when that hold came from a route
    /// that carries no operation name, which is itself the answer rather
    /// than a missing reading (#2793).
    pub max_completed_hold_operation: Option<&'static str>,
    /// Dedicated reader connections discarded on return for age, checkout
    /// count, failed cleanup/health checks, or an explicitly non-reusable lease.
    /// Counts replacement attempts, including opens that fail; not pool shutdown.
    pub reader_discards: u64,
    /// A discarded pooled-reader return (expiry or reset/pristine-check failure)
    /// whose replacement connection then also failed to open, permanently
    /// shrinking the physical pool by one slot below `max_readers`. Logged at
    /// `warn` when it happens; this counter makes the shrink observable in a
    /// snapshot too, since the pool itself never re-grows on its own.
    pub reader_replacement_open_failures: u64,
}

/// The longest completed pooled-reader hold and the operation that held it,
/// kept under one lock so a snapshot cannot pair one checkout's duration with
/// another's name.
#[derive(Debug, Default, Clone, Copy)]
struct LongestCompletedHold {
    micros: u64,
    operation: Option<&'static str>,
}

#[derive(Debug, Default)]
pub(super) struct ReaderAcquisitionCounters {
    pooled_checkouts: AtomicU64,
    standalone_opens: AtomicU64,
    infrastructure_standalone_opens: AtomicU64,
    checkout_timeouts: AtomicU64,
    busy_timeouts: AtomicU64,
    active_pooled_checkouts: AtomicU64,
    peak_active_pooled_checkouts: AtomicU64,
    completed_pooled_checkouts: AtomicU64,
    longest_completed_hold: parking_lot::Mutex<LongestCompletedHold>,
    pub(super) reader_discards: AtomicU64,
    reader_replacement_open_failures: AtomicU64,
}

impl ReaderAcquisitionCounters {
    pub(super) fn record_pooled_checkout(&self) {
        self.pooled_checkouts.fetch_add(1, Ordering::Relaxed);
        let active = self
            .active_pooled_checkouts
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.peak_active_pooled_checkouts
            .fetch_max(active, Ordering::Relaxed);
    }

    pub(super) fn record_checkout_timeout(&self) {
        self.checkout_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_busy_timeout(&self) {
        self.busy_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_reader_replacement_open_failure(&self) {
        self.reader_replacement_open_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_standalone_open(&self, purpose: StandaloneReaderPurpose) {
        if purpose.is_infrastructure() {
            self.infrastructure_standalone_opens
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.standalone_opens.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn record_checkout_completed(
        &self,
        hold: Duration,
        operation: Option<&'static str>,
    ) {
        let previous = self.active_pooled_checkouts.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "reader active-checkout counter underflow");
        self.completed_pooled_checkouts
            .fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(hold.as_micros()).unwrap_or(u64::MAX);
        // The maximum and the name of what held it are one reading: taken
        // apart they can report a duration from one checkout beside a label
        // from another, which is worse than no label at all.
        let mut longest = self.longest_completed_hold.lock();
        if micros > longest.micros {
            longest.micros = micros;
            longest.operation = operation;
        }
    }

    pub(super) fn snapshot(
        &self,
        reader_admission_capacity: usize,
        available_reader_admission_slots: usize,
    ) -> ReaderAcquisitionSnapshot {
        let pooled_checkouts = self.pooled_checkouts.load(Ordering::Relaxed);
        let standalone_opens = self.standalone_opens.load(Ordering::Relaxed);
        let longest_completed_hold = *self.longest_completed_hold.lock();
        ReaderAcquisitionSnapshot {
            reader_admission_capacity,
            available_reader_admission_slots,
            acquisitions: pooled_checkouts.saturating_add(standalone_opens),
            pooled_checkouts,
            standalone_opens,
            infrastructure_standalone_opens: self
                .infrastructure_standalone_opens
                .load(Ordering::Relaxed),
            checkout_timeouts: self.checkout_timeouts.load(Ordering::Relaxed),
            busy_timeouts: self.busy_timeouts.load(Ordering::Relaxed),
            active_pooled_checkouts: self.active_pooled_checkouts.load(Ordering::Relaxed),
            peak_active_pooled_checkouts: self.peak_active_pooled_checkouts.load(Ordering::Relaxed),
            completed_pooled_checkouts: self.completed_pooled_checkouts.load(Ordering::Relaxed),
            max_completed_hold_micros: longest_completed_hold.micros,
            max_completed_hold_operation: longest_completed_hold.operation,
            reader_discards: self.reader_discards.load(Ordering::Relaxed),
            reader_replacement_open_failures: self
                .reader_replacement_open_failures
                .load(Ordering::Relaxed),
        }
    }
}
