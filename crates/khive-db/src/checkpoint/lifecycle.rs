//! Session sweeps, lifecycle emission, and checkpoint connection ownership.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::pool::ConnectionPool;

#[cfg(doc)]
use super::run_checkpoint_task;
use super::{
    log_tx_age_emission, ReaderCheckoutSweepState, SessionSweepConfig, TxAgeSweepState,
    WalpinSidecarState, CHECKPOINT_LIFECYCLE_APPEND_ATTEMPTS, CHECKPOINT_LIFECYCLE_APPEND_FAILURES,
    CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS,
};

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
    pool: Arc<ConnectionPool>,
    filter: khive_storage::tx_registry::TxOriginFilter,
    tx_age_state: TxAgeSweepState,
    reader_checkout_state: ReaderCheckoutSweepState,
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
            pool: backend.pool,
            filter,
            tx_age_state: TxAgeSweepState::default(),
            reader_checkout_state: ReaderCheckoutSweepState::default(),
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
            sweep
                .reader_checkout_state
                .sweep_at(&sweep.pool, Instant::now());
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
pub(super) struct CheckpointPressureEpisode {
    pub(super) elevated_ticks: u64,
    pub(super) peak_wal_pages: u64,
}

impl CheckpointPressureEpisode {
    pub(super) fn start(wal_pages: u64) -> Self {
        Self {
            elevated_ticks: 1,
            peak_wal_pages: wal_pages,
        }
    }

    pub(super) fn observe(&mut self, wal_pages: u64) {
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
pub(super) struct CheckpointLifecycleEmitter {
    namespace: Option<String>,
    sender: Option<tokio::sync::mpsc::Sender<khive_storage::Event>>,
    worker: Option<tokio::task::JoinHandle<()>>,
    busy_warning_emitted: bool,
}

impl CheckpointLifecycleEmitter {
    pub(super) fn new(owner: Option<CheckpointLifecycleOwner>) -> Self {
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
                    tracing::warn!(target: "khive_db::checkpoint",
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
    pub(super) fn try_emit<P: serde::Serialize>(
        &mut self,
        kind: khive_types::EventKind,
        payload: P,
    ) -> bool {
        let (Some(namespace), Some(sender)) = (&self.namespace, &self.sender) else {
            return true;
        };
        let payload_value = match serde_json::to_value(&payload) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(target: "khive_db::checkpoint",
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
                    tracing::warn!(target: "khive_db::checkpoint",
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
                tracing::warn!(target: "khive_db::checkpoint",
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
    pub(super) async fn shutdown(mut self) {
        drop(self.sender.take());
        let Some(worker) = self.worker.take() else {
            return;
        };
        worker.abort();
        match worker.await {
            Ok(()) => {}
            Err(err) if err.is_cancelled() => {}
            Err(err) => tracing::warn!(target: "khive_db::checkpoint",
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
pub(super) struct CheckpointConnection {
    pub(super) conn: Option<crate::pool::CheckpointFile>,
    /// Consecutive failed `open_checkpoint_file` attempts since the last
    /// successful open (or since task startup). Drives the WARN-once /
    /// debug-thereafter log rate-limiting in `ensure_open`: a file-backed
    /// pool that transiently loses its dedicated connection would otherwise
    /// log a WARN on every tick (default 500ms) for as long as the outage
    /// lasts, which for a read-only or in-memory pool — where the open can
    /// never succeed — means permanent per-tick WARN spam.
    consecutive_open_failures: u32,
}

impl CheckpointConnection {
    pub(super) fn new() -> Self {
        Self {
            conn: None,
            consecutive_open_failures: 0,
        }
    }

    /// Ensure a usable connection is open, lazily (re)opening from `pool`
    /// when the current one is absent. Reuses the crate's existing untracked
    /// fixed-operation connection factory (`ConnectionPool::open_checkpoint_file`),
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
    pub(super) fn ensure_open(
        &mut self,
        pool: &ConnectionPool,
    ) -> Option<&crate::pool::CheckpointFile> {
        if self.conn.is_none() {
            match pool.open_checkpoint_file() {
                Ok(conn) => {
                    if self.consecutive_open_failures > 0 {
                        tracing::info!(target: "khive_db::checkpoint",
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
                        tracing::warn!(target: "khive_db::checkpoint",
                            error = %e,
                            "failed to open the dedicated checkpoint connection; \
                             this tick is skipped and the open retried next tick"
                        );
                    } else {
                        tracing::debug!(target: "khive_db::checkpoint",
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
