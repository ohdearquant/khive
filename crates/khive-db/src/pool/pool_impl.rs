//! Pool diagnostics, writer-task lifecycle and connection maintenance.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_storage::error::StorageError;
use khive_storage::tx_registry::TxOrigin;
use khive_storage::StorageCapability;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::Semaphore;

use crate::database_owner_identity::{DatabaseOwnerIdentity, DatabaseOwnerIdentityError};
use crate::disk_guard_config::EffectiveDiskGuardConfig;
#[cfg(test)]
use crate::disk_guard_config::{DiskGuardConfigSource, DEFAULT_DISK_GUARD_DEADLINE_MS};
use crate::error::SqliteError;
#[cfg(any(unix, windows))]
use crate::file_identity::{database_file_identity, DatabaseFileIdentity};
use crate::writer_task::WriterTaskHandle;

#[cfg(any(unix, windows))]
use super::opened_sqlite_file_identity;
#[cfg(doc)]
use super::pool_exhausted_error;
#[cfg(feature = "namespace-trigram-proto")]
use super::register_namespace_trigram;
use super::{
    claimed_file_identity, close_connection_quietly, configure_reader_connection,
    current_journal_mode, open_reader_connection, read_database_id, reader_connection_is_healthy,
    register_read_functions, register_writer_clock, reset_reader_connection, CheckpointFile,
    ConnectionPool, PoolConfig, PoolIdentityRegistration, PooledReader, ReaderAcquisitionSnapshot,
    ReaderGuard, RuntimeWriteOperation, SearchMechanismSnapshot, StandaloneCheckpoint,
    StandaloneReaderPurpose, StandaloneWriteUnit, WriteAdmission, WriterAcquisitionCounters,
    WriterAcquisitionSnapshot, NEXT_MAIN_POOL_GENERATION,
};
#[cfg(test)]
use super::{run_identity_open_hook, IdentityOpenStage};

impl ConnectionPool {
    /// Snapshot all instrumented writer acquisition outcomes since this pool
    /// was constructed.
    pub fn writer_acquisition_snapshot(&self) -> WriterAcquisitionSnapshot {
        self.writer_acquisition_counters
            .snapshot(self.write_admission.lease_timeouts())
    }

    /// Count a coordinator call only after served-kind filtering selects this
    /// backend. The canonical requested kind is the granular kind when one was
    /// supplied, or the entity/note substrate otherwise.
    pub fn record_search_dispatch(&self, backend_id: &str, requested_kind: &str) {
        let mut dispatches = self.search_dispatches.lock();
        let count = dispatches
            .entry(backend_id.to_owned())
            .or_default()
            .entry(requested_kind.to_owned())
            .or_default();
        *count = count.saturating_add(1);
    }

    /// Count one actual candidate note row returned at the post-fusion
    /// hydration seam, including rows later filtered as deleted. Absent rows
    /// and later result metadata are excluded.
    pub fn record_note_candidate_hydration_row(&self) {
        let _ = self.note_candidate_hydration_rows.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| Some(current.saturating_add(1)),
        );
    }

    /// Snapshot ADR-166 G4/G5 counters. Values reset only with this pool.
    pub fn search_mechanism_snapshot(&self) -> SearchMechanismSnapshot {
        SearchMechanismSnapshot {
            dispatches_by_backend_and_kind: self.search_dispatches.lock().clone(),
            note_candidate_hydration_rows: self
                .note_candidate_hydration_rows
                .load(Ordering::Relaxed),
        }
    }

    /// Snapshot reader acquisition, saturation, and hold lifecycle outcomes
    /// since this pool was constructed. Counters reset only with pool
    /// reconstruction.
    pub fn reader_acquisition_snapshot(&self) -> ReaderAcquisitionSnapshot {
        self.reader_acquisition_counters.snapshot(
            self.max_readers.max(1),
            self.sql_bridge_reader_slots.available_permits(),
        )
    }

    /// Record one pool-wide reader-admission wait that exhausted the configured
    /// checkout timeout outside [`Self::reader_until`] (currently the explicit
    /// raw-SQL read-transaction exception and reads on a standalone writer).
    pub(crate) fn record_reader_admission_timeout(&self) {
        self.reader_acquisition_counters.record_checkout_timeout();
    }

    /// Count a query error from an already checked-out pooled reader when it is
    /// SQLite's `SQLITE_BUSY` surfacing after the busy handler gave up. Every
    /// other error, including `SQLITE_LOCKED`, is ignored. Checkout exhaustion
    /// is counted by [`Self::reader_until`] before any query runs, so the two
    /// classes cannot overlap.
    pub(crate) fn record_reader_query_error(&self, error: &StorageError) {
        if crate::read_cancellation::storage_error_sqlite_code(error)
            == Some(rusqlite::ErrorCode::DatabaseBusy)
        {
            self.reader_acquisition_counters.record_busy_timeout();
        }
    }

    /// Clone the pool-scoped counter set for the lifetime-owned writer task.
    pub(crate) fn writer_acquisition_counters(&self) -> Arc<WriterAcquisitionCounters> {
        Arc::clone(&self.writer_acquisition_counters)
    }

    pub(crate) fn write_admission(&self) -> Arc<WriteAdmission> {
        Arc::clone(&self.write_admission)
    }

    pub fn effective_disk_guard_config(&self) -> Option<EffectiveDiskGuardConfig> {
        self.disk_guard_config
    }

    #[cfg(test)]
    pub(crate) fn set_test_write_admission(
        &mut self,
        floor_bytes: u64,
        probe: impl Fn(&Path) -> std::io::Result<u64> + Send + Sync + 'static,
    ) {
        let database_path = if self.config.read_only {
            None
        } else {
            self.canonical_path().map(Path::to_path_buf)
        };
        let admission = Arc::new(
            WriteAdmission::new(
                database_path,
                floor_bytes,
                DEFAULT_DISK_GUARD_DEADLINE_MS,
                self.config.volume_lock_dir.clone(),
            )
            .expect("test admission volume identity"),
        );
        admission.set_test_space_probe(probe);
        self.write_admission = admission;
        if self.canonical_path().is_some() && !self.config.read_only {
            self.disk_guard_config = Some(EffectiveDiskGuardConfig {
                reserve_bytes: floor_bytes,
                guard_deadline_ms: DEFAULT_DISK_GUARD_DEADLINE_MS,
                reserve_source: DiskGuardConfigSource::Backend,
                deadline_source: DiskGuardConfigSource::Default,
                legacy_environment_present: false,
            });
        }
    }

    /// Get the current number of available reader connections.
    pub fn available_readers(&self) -> usize {
        self.readers.len()
    }

    /// Get the total number of reader connections in the pool.
    pub fn max_readers(&self) -> usize {
        self.max_readers
    }

    /// Age of the oldest outstanding pooled reader checkout, including
    /// degraded shared-reader leases and their reset/replacement work.
    /// Returns `None` when no checkout remains; standalone readers are
    /// separately tracked by the transaction registry.
    pub fn oldest_checkout_age(&self) -> Option<Duration> {
        self.oldest_checkout_age_at(Instant::now())
    }

    pub(crate) fn oldest_checkout_age_at(&self, now: Instant) -> Option<Duration> {
        self.reader_checkouts
            .lock()
            .first_key_value()
            .map(|(started, _)| now.saturating_duration_since(*started))
    }

    pub(super) fn begin_reader_checkout(&self) -> Instant {
        let started = Instant::now();
        // Instant resolution can give simultaneous leases the same key.
        *self.reader_checkouts.lock().entry(started).or_default() += 1;
        started
    }

    pub(super) fn finish_reader_checkout(&self, started: Instant) {
        let mut checkouts = self.reader_checkouts.lock();
        if let Some(count) = checkouts.get_mut(&started) {
            *count -= 1;
            if *count == 0 {
                checkouts.remove(&started);
            }
        }
    }

    /// Return the pool configuration.
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Observe actual SQLite statement starts on this private test pool.
    ///
    /// The limit bounds retained SQL records. Failed steps count as attempts;
    /// preparation alone does not count. The guard observes every pool-owned
    /// connection, including the queued writer. Do not run unrelated background
    /// work on the fixture pool; see the guard documentation for limitations.
    /// Connection setup runs before observation begins on each connection and
    /// is never recorded, including for opens during an active observation.
    /// Reader connection opens use the existing reader acquisition counters instead.
    #[cfg(any(test, feature = "test-support"))]
    pub fn observe_test_statement_starts(
        &self,
        limit: usize,
    ) -> Result<crate::statement_observer::StatementStartObservation, SqliteError> {
        self.statement_observer.observe(limit)
    }

    /// Identify this pool's counter window when it is designated as main.
    /// Repeated runtime handles and diagnostics reads reuse the same generation;
    /// constructing secondary pools does not consume main-pool generations.
    pub fn main_pool_generation(&self) -> u64 {
        *self.main_pool_generation.get_or_init(|| {
            NEXT_MAIN_POOL_GENERATION
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .expect("main pool generation exhausted")
        })
    }

    /// The typed admission failure for a pooled reader checkout that
    /// exhausted `checkout_timeout`: no reader was acquired, so the
    /// operation never started and a retry cannot duplicate a side effect.
    pub(crate) fn reader_admission_timeout(&self, operation: &'static str) -> StorageError {
        StorageError::AdmissionTimeout {
            operation: operation.into(),
            timeout_ms: u64::try_from(self.config.checkout_timeout.as_millis()).unwrap_or(u64::MAX),
            pool_identity: Some(
                self.identity_registration
                    .as_ref()
                    .map(PoolIdentityRegistration::label)
                    .unwrap_or_else(|| ":memory:".to_string()),
            ),
        }
    }

    /// Resolve a [`Self::reader_until`] outcome into a checked-out guard or
    /// the canonical refusal. This is the single home of the checkout
    /// tri-state; call sites must not re-derive any arm of it:
    ///
    /// - `Ok(Some)` — a reader was checked out.
    /// - `Ok(None)` — `should_stop()` fired: the request was cancelled or hit
    ///   its deadline before checkout. NOT an admission wait, so it maps to
    ///   the non-retryable [`StorageError::Timeout`] — emitting the retryable
    ///   `AdmissionTimeout` here would invite an immediate retry of a request
    ///   its caller already abandoned, into a possibly saturated pool.
    /// - `Err` carrying the pool's own `SQLITE_BUSY` — `reader_until` executes
    ///   no SQL, so the only `SQLITE_BUSY` it can produce is
    ///   [`pool_exhausted_error`], raised when `checkout_timeout` elapses with
    ///   no reader available. A genuine admission wait that ended before any
    ///   work began maps to the retryable [`StorageError::AdmissionTimeout`].
    /// - any other `Err` (e.g. [`Self::ensure_pooled_writer_active`] returning
    ///   `InvalidData` for a retired pooled writer) — an opaque driver failure
    ///   under the caller's capability, non-retryable.
    pub(crate) fn resolve_reader_checkout<'p>(
        &self,
        capability: StorageCapability,
        operation: &'static str,
        outcome: Result<Option<ReaderGuard<'p>>, SqliteError>,
    ) -> Result<ReaderGuard<'p>, StorageError> {
        match outcome {
            Ok(Some(mut guard)) => {
                guard.label_operation(operation);
                Ok(guard)
            }
            Ok(None) => Err(self.reader_checkout_refusal(capability, operation, None)),
            Err(error) => Err(self.reader_checkout_refusal(capability, operation, Some(error))),
        }
    }

    /// The refusal for a reader checkout that produced nothing, shared by
    /// [`Self::resolve_reader_checkout`] and [`Self::acquire_reader_admission`]
    /// so the arms documented on the former have one home. `None` is the
    /// stopped-request arm; `Some` carries the failed checkout's error.
    pub(super) fn reader_checkout_refusal(
        &self,
        capability: StorageCapability,
        operation: &'static str,
        error: Option<SqliteError>,
    ) -> StorageError {
        let Some(error) = error else {
            return StorageError::Timeout {
                operation: operation.into(),
            };
        };
        let is_pool_exhausted = matches!(
            &error,
            SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        );
        if is_pool_exhausted {
            self.reader_admission_timeout(operation)
        } else {
            StorageError::driver(capability, operation, error)
        }
    }

    /// Pool-wide admission permits shared by pooled readers, the explicit
    /// raw-SQL read-transaction exception, and reads on standalone writers.
    pub(crate) fn sql_bridge_reader_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sql_bridge_reader_slots)
    }

    /// Pool-wide permit for a file-backed raw-SQL writer handle.
    pub(crate) fn sql_bridge_writer_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sql_bridge_writer_slots)
    }

    /// Current writer holds that prevent voluntary daemon retirement.
    ///
    /// The raw-SQL writer permit remains handle-scoped even in autocommit.
    /// This read acquires no connection and changes no admission policy.
    pub fn retirement_writer_holds(&self) -> usize {
        usize::from(self.writer.is_locked())
            + usize::from(self.sql_bridge_writer_slots.available_permits() == 0)
    }

    /// This pool's ADR-091 backend-scoped attribution origin (ADR-091,
    /// backend-scoped WAL-pin attribution design note): `Database(_)` for a
    /// file-backed pool, `Memory` for an in-memory pool. Every
    /// `tx_registry::register_scoped` call site threaded in this crate
    /// passes this value as the span's origin.
    pub fn origin(&self) -> TxOrigin {
        self.origin.clone()
    }

    /// The canonical path this pool's `origin()` identity was minted from,
    /// `None` for an in-memory pool. `DbIdentity` has no path accessor by
    /// design; sidecar derivation and other filesystem consumers use this —
    /// the same canonical value the identity was minted from — instead of
    /// re-deriving a path from the raw configured one.
    pub fn canonical_path(&self) -> Option<&Path> {
        self.identity_path.as_deref()
    }

    /// Unix file identity retained for callers using the original tuple API.
    #[cfg(unix)]
    pub fn opened_file_identity(&self) -> Option<(u64, u64)> {
        self.opened_file_identity
            .map(DatabaseFileIdentity::unix_parts)
    }

    /// Physical file identity pinned to SQLite's opened main file. Construction
    /// compares it with the path on both sides of the open before admission.
    #[cfg(any(unix, windows))]
    pub fn opened_file_identity_record(&self) -> Option<DatabaseFileIdentity> {
        self.opened_file_identity
    }

    /// Ownership evidence captured through the database SQLite opened.
    /// This does not select the topology's main backend or establish a root binding.
    /// Legacy read-only and low-space opens without a stored UUID must reopen
    /// after installation; this accessor never installs or re-mints an identity.
    pub fn database_owner_identity(
        &self,
    ) -> Result<DatabaseOwnerIdentity, DatabaseOwnerIdentityError> {
        if self.identity_path.is_none() {
            return Err(DatabaseOwnerIdentityError::InMemory);
        }
        #[cfg(any(unix, windows))]
        {
            let durable_id = self
                .opened_database_id
                .ok_or(DatabaseOwnerIdentityError::DurableIdentityUnavailable)?;
            let file_identity = self
                .opened_file_identity
                .ok_or(DatabaseOwnerIdentityError::PhysicalIdentityUnavailable)?;
            Ok(DatabaseOwnerIdentity {
                durable_id,
                file_identity,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(DatabaseOwnerIdentityError::UnsupportedPlatform)
        }
    }

    /// Require both this opened database's UUID and physical file to match.
    pub fn verify_database_owner(
        &self,
        expected: &DatabaseOwnerIdentity,
    ) -> Result<(), DatabaseOwnerIdentityError> {
        self.database_owner_identity()?.verify_owner(expected)
    }

    /// Whether the write queue is effectively enabled for this pool: the
    /// resolved `write_queue_enabled` flag AND file-backed.
    ///
    /// `ConnectionPool::new` resolves the "no preference" (`None`) preference
    /// to a concrete `Some(..)` once `path` is known, so every reader of
    /// `config.write_queue_enabled` sees a resolved value; the `debug_assert`
    /// pins that invariant and a `None` that slipped past would read as
    /// disabled. Bypassing `ConnectionPool::new` to construct a pool is a
    /// construction-path bug. Use this instead of repeating
    /// `config().write_queue_enabled.unwrap_or(false) && config().path.is_some()`
    /// at every routing/violation site.
    pub fn write_queue_active(&self) -> bool {
        debug_assert!(
            self.config.write_queue_enabled.is_some(),
            "write_queue_enabled must be resolved to Some(..) by ConnectionPool::new \
             before any write_queue_active read"
        );
        self.config.write_queue_enabled.unwrap_or(false) && self.config.path.is_some()
    }

    /// Whether a writer-task JoinHandle has been stored at least once.
    ///
    /// Unlike [`Self::take_writer_task_join`], this remains true after the
    /// one-shot handle slot is emptied, distinguishing a task that never
    /// spawned from a handle another caller already consumed.
    pub fn writer_task_join_was_stored(&self) -> bool {
        self.writer_task_join_stored.load(Ordering::SeqCst)
    }

    /// Return the pool-wide ADR-067 Component A writer task, spawning it
    /// lazily on first access if `PoolConfig::write_queue_enabled` is set.
    /// Exactly one writer task exists per `ConnectionPool` (per DB file); see
    /// crates/khive-db/docs/api/pool.md#connectionpoolwriter_task_handle--single-writer-task-rationale
    /// for why a per-store writer task would defeat the single-writer
    /// guarantee.
    ///
    /// Returns `Ok(None)` if the flag is off, or if the writer task failed to
    /// spawn for a reason other than a missing runtime (for example, an
    /// in-memory pool has no standalone-connection support) — callers fall
    /// back to the legacy pool-mutex write path in either case. A spawn
    /// failure is logged once here (at first access), not once per store.
    ///
    /// Returns `Err(StorageError::WriterTaskNoRuntime)` instead of panicking
    /// when `write_queue_enabled` is set but this is the first access and no
    /// Tokio runtime is available on the calling thread (checked via
    /// [`tokio::runtime::Handle::try_current`]) — spawning the writer task
    /// requires `tokio::spawn`, which panics outside a runtime. Callers that
    /// already treat a missing writer task as best-effort (construction-time
    /// degrade to the legacy path, matching slice 1's documented policy) can
    /// collapse this into `None` with `.ok().flatten()`; callers that need to
    /// fail loud on a genuine misconfiguration (write queue requested but no
    /// runtime to run it on) can propagate the `Err` directly.
    pub fn writer_task_handle(&self) -> Result<Option<WriterTaskHandle>, StorageError> {
        // Same pinned invariant `write_queue_active` asserts, kept inline
        // here because this gate keys on the flag ALONE: an explicit
        // `Some(true)` on an in-memory pool must still attempt the spawn
        // and degrade (documented + tested in
        // `explicit_true_stays_on_for_memory_backed_pool`), so the
        // file-backed half of `write_queue_active` cannot gate this early
        // return.
        debug_assert!(
            self.config.write_queue_enabled.is_some(),
            "write_queue_enabled must be resolved to Some(..) by ConnectionPool::new \
             before any writer_task_handle read"
        );
        if !self.config.write_queue_enabled.unwrap_or(false) {
            return Ok(None);
        }
        // Fast path: already resolved (spawned, degraded, or off) by an
        // earlier call — no need to re-check the runtime.
        if let Some(existing) = self.writer_task.get() {
            return Ok(existing.clone());
        }
        // Not yet initialized and the flag is on: spawning requires
        // `tokio::spawn`, which panics outside a runtime context. Check
        // first and fail loud with a typed error instead.
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(StorageError::WriterTaskNoRuntime);
        }
        Ok(self
            .writer_task
            .get_or_init(|| {
                #[cfg(test)]
                self.writer_task_spawn_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                match crate::writer_task::spawn(self, self.config.write_queue_capacity) {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        tracing::warn!(target: "khive_db::pool",
                            error = %e,
                            "KHIVE_WRITE_QUEUE=1 but the writer task failed to spawn; \
                             writes fall back to the pool-mutex path"
                        );
                        None
                    }
                }
            })
            .clone())
    }

    pub(crate) fn writer_admission_snapshot(
        &self,
    ) -> Result<crate::writer_admission_metrics::Snapshot, &'static str> {
        if !self.config.write_queue_enabled.unwrap_or(false) {
            return Err("writer_queue_disabled");
        }
        match self.writer_task.get() {
            None => Err("writer_not_initialized"),
            Some(None) => Err("writer_unavailable"),
            Some(Some(handle)) => handle
                .admission_snapshot()
                .map_err(|reason| reason.reason()),
        }
    }

    /// Resolve the writer task for a store write at the moment the write is
    /// issued, rather than trusting only a handle cached by a synchronous
    /// store constructor. Construction can legitimately run before Tokio is
    /// entered, in which case `writer_task_handle()` returns
    /// `WriterTaskNoRuntime` without caching a terminal `None`.
    ///
    /// Strict routing makes every missing handle fail closed here. The
    /// caller remains responsible for recording a non-strict direct fallback
    /// at the exact fallback seam with [`Self::record_direct_route`].
    pub(crate) fn writer_task_for_write(
        &self,
        cached: Option<&WriterTaskHandle>,
        operation: &'static str,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        let handle = match cached {
            Some(handle) => Some(handle.clone()),
            None => match self.writer_task_handle() {
                Ok(handle) => handle,
                Err(error) if self.config.write_routing_strict => return Err(error),
                Err(_) => None,
            },
        };

        if handle.is_none() && self.config.write_routing_strict {
            return Err(StorageError::Pool {
                operation: operation.into(),
                message: "strict write routing requires a writer-task handle; no handle is \
                          available, so the direct writer fallback was refused"
                    .into(),
            });
        }
        Ok(handle)
    }

    /// Record one actual compatibility fallback around the writer task. A
    /// file-backed pool with the queue enabled should never reach this seam
    /// in strict mode because [`Self::writer_task_for_write`] refuses first.
    pub(crate) fn record_direct_route(&self, site: crate::timeout_sink::Site) {
        if self.write_queue_active() {
            crate::timeout_sink::emit_direct_route_violation(
                &crate::timeout_sink::db_label(self),
                site,
            );
        }
    }

    /// Resolve a runtime-owned transaction through the same strict/compatibility
    /// policy as store writes. `None` permits the caller's direct transaction and
    /// records its compatibility fallback when the file-backed queue is enabled.
    /// A strict refusal returns before any direct-writer acquisition or telemetry.
    pub fn writer_task_for_runtime_write(
        &self,
        operation: RuntimeWriteOperation,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        let handle = self.writer_task_for_write(None, operation.operation())?;
        if handle.is_none() {
            self.record_direct_route(operation.fallback_site());
        }
        Ok(handle)
    }

    /// Test-only: how many times the writer-task init closure actually ran.
    /// Must be at most 1 for the pool's whole lifetime, regardless of how
    /// many times [`Self::writer_task_handle`] is called or how many stores
    /// are constructed over this pool.
    #[cfg(test)]
    pub(crate) fn writer_task_spawn_count(&self) -> usize {
        self.writer_task_spawn_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Record the writer task's `tokio::spawn` JoinHandle. Called exactly
    /// once, by [`crate::writer_task::spawn`], immediately after spawning —
    /// the same `writer_task` OnceLock init that makes spawn at-most-once
    /// per pool makes this write at-most-once per pool.
    ///
    /// First-wins: if a handle was ever stored (including one a caller has
    /// since taken — `writer_task_join_stored` remembers), the existing
    /// state is kept and the new handle is dropped (dropping a `JoinHandle`
    /// detaches its task without cancelling it). A second store violates the
    /// at-most-once contract and trips the debug_assert in debug builds;
    /// release builds keep the first handle rather than silently swapping
    /// the drain owner out from under whichever caller already took it.
    pub(crate) fn set_writer_task_join(&self, join: tokio::task::JoinHandle<()>) {
        // `swap(true)` returns the prior value: `true` means a handle was
        // stored at least once before, so this is a second store — even when
        // the slot itself is empty because `take_writer_task_join` already
        // ran (the slot alone cannot tell "never stored" from "taken").
        let first_store = !self.writer_task_join_stored.swap(true, Ordering::SeqCst);
        debug_assert!(
            first_store,
            "writer task JoinHandle stored twice (even counting a taken one); \
             the writer_task OnceLock is supposed to make spawn at-most-once per pool"
        );
        if first_store {
            *self.writer_task_join.lock() = Some(join);
        }
    }

    /// Take the writer task's JoinHandle, if a writer task was spawned and
    /// the handle has not already been taken.
    ///
    /// Intended for short-lived batch callers that drop every
    /// [`WriterTaskHandle`] clone (closing the queue) and then need to await
    /// the task's exit before treating the database file as settled: the
    /// task's connection close fires SQLite's close-time WAL checkpoint, so
    /// until the task exits the file bytes can still move after the caller's
    /// last write returned.
    ///
    /// One-shot: `None` means either the write queue never spawned
    /// (disabled, or spawn degraded) or another caller already took the
    /// handle — in both cases there is nothing further to await here.
    /// Exactly one subsystem may own the drain: the single caller that
    /// receives `Some(_)` is the sole owner of the task-exit await (and of
    /// the close-time WAL checkpoint that settles the database file); every
    /// later caller receives `None` and must not arrange its own await.
    pub fn take_writer_task_join(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.writer_task_join.lock().take()
    }

    pub(super) fn open_pooled_reader(&self) -> Result<Box<PooledReader>, SqliteError> {
        let opened_at = Instant::now();
        // Allocate once per physical reader; checkouts move the same box between
        // the queue and lease without allocating on every borrow/return.
        Ok(Box::new(PooledReader {
            conn: self.open_reader_connection()?,
            opened_at,
            checkouts: 0,
        }))
    }

    pub(super) fn open_reader_connection(&self) -> Result<Connection, SqliteError> {
        let path = self.read_connection_path()?;
        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_opened_file_identity(identity_path)?;
        }
        let conn = open_reader_connection(path, &self.config)?;
        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_connection_file_identity(&conn, identity_path)?;
        }
        self.verify_opened_database_id(&conn)?;
        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(&conn, &self.statement_observer)?;
        Ok(conn)
    }

    fn read_connection_path(&self) -> Result<&Path, SqliteError> {
        self.read_only_open_target
            .as_deref()
            .or(self.identity_path.as_deref())
            .ok_or_else(|| {
                SqliteError::InvalidData(
                    "in-memory databases do not support standalone connections".to_string(),
                )
            })
    }

    /// Construct the complete admitted owner before the connection can leave
    /// this module. The native opener below is private to these factories.
    #[track_caller]
    pub(super) fn standalone_write_unit(&self) -> Result<StandaloneWriteUnit, SqliteError> {
        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "database is read-only: standalone write transactions are not permitted".into(),
            ));
        }
        let volume_lease = self.write_admission.acquire()?;
        let conn = self.open_standalone_connection()?;
        self.writer_acquisition_counters
            .standalone_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        let unit = StandaloneWriteUnit::new(conn, Arc::clone(&self.write_admission), volume_lease);
        if !unit.conn().is_autocommit() {
            return Err(SqliteError::WriterSettlementUnknown);
        }
        Ok(unit)
    }

    /// Retain a dedicated recovery connection without granting ordinary SQL.
    /// This bypasses logical-write admission so a checkpoint can reclaim space.
    pub fn open_standalone_checkpoint(&self) -> Result<StandaloneCheckpoint<'_>, SqliteError> {
        Ok(StandaloneCheckpoint::new(
            self,
            self.open_checkpoint_file()?,
        ))
    }

    pub(crate) fn open_checkpoint_file(&self) -> Result<CheckpointFile, SqliteError> {
        self.open_standalone_connection().map(CheckpointFile::new)
    }

    pub(crate) fn spawn_writer_task(
        &self,
        capacity: usize,
    ) -> Result<WriterTaskHandle, SqliteError> {
        let conn = self.open_standalone_connection()?;
        crate::writer_task::spawn_with_connection(self, conn, capacity)
    }

    pub(crate) fn open_sql_bridge_writer(
        self: Arc<Self>,
        slot: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<Box<dyn khive_storage::SqlWriter>, StorageError> {
        let conn = self
            .open_standalone_connection()
            .map_err(|error| error.into_storage_error(StorageCapability::Sql, "open_writer"))?;
        self.writer_acquisition_counters
            .standalone_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        crate::sql_bridge::build_standalone_writer(self, conn, slot)
    }

    pub(crate) fn inspect_diagnostic_connection(
        &self,
        collect_size_composition: bool,
    ) -> crate::diagnostics::PoolInspection {
        crate::diagnostics::inspect_open_connection(
            self,
            self.open_standalone_connection(),
            collect_size_composition,
        )
    }

    pub(crate) fn inspect_diagnostic_connection_interruptibly(
        &self,
        scope: &crate::read_cancellation::InterruptibleReadScope,
        collect_size_composition: bool,
    ) -> Result<crate::diagnostics::PoolInspection, StorageError> {
        scope.ensure_active()?;
        crate::diagnostics::inspect_open_connection_interruptibly(
            self,
            self.open_standalone_connection(),
            scope,
            collect_size_composition,
        )
    }

    /// FTS merge INSERTs are logical writes, even when scheduled by the
    /// checkpoint task. They use an admitted owner, never its recovery file.
    pub(crate) fn run_fts_maintenance(
        &self,
        config: &crate::fts_maintenance::FtsMaintenanceConfig,
        state: &mut crate::fts_maintenance::FtsMaintenanceState,
        now: Instant,
    ) -> Result<Option<crate::fts_maintenance::FtsMaintenanceStep>, String> {
        if !state.is_due(config, now) {
            return Ok(None);
        }
        let unit = self
            .standalone_write_unit()
            .map_err(|error| format!("FTS maintenance admission: {error}"))?;
        crate::fts_maintenance::run_if_due_with_admission(unit.conn(), config, state, now, || {
            self.write_admission
                .check()
                .map_err(|error| format!("FTS maintenance admission: {error}"))
        })
    }

    /// The sole raw standalone opener. Only the complete-consumer factories
    /// above can call it; no raw connection is returned across that boundary.
    fn open_standalone_connection(&self) -> Result<Connection, SqliteError> {
        let path = self.identity_path.as_deref().ok_or_else(|| {
            SqliteError::InvalidData(
                "in-memory databases do not support standalone connections".to_string(),
            )
        })?;

        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "database is read-only: standalone write connections are not permitted".to_string(),
            ));
        }

        // The configured spelling may be a symlink that changed since this
        // pool opened. Use its pinned target, and refuse replacement of that
        // target before SQLite can execute the diagnostics PASSIVE checkpoint.
        #[cfg(any(unix, windows))]
        self.verify_opened_file_identity(path)?;

        #[cfg(test)]
        run_identity_open_hook(path, IdentityOpenStage::BeforeStandaloneOpen, None);

        let conn = claimed_file_identity::open_connection(
            &self.config,
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
            self.identity_path.as_deref(),
        )?;
        #[cfg(test)]
        run_identity_open_hook(path, IdentityOpenStage::AfterStandaloneOpen, Some(&conn));
        #[cfg(any(unix, windows))]
        self.verify_connection_file_identity(&conn, path)?;
        self.verify_opened_database_id(&conn)?;
        self.configure_standalone_connection(&conn)?;
        Ok(conn)
    }

    /// Configure a caller-owned native connection. This does not open or
    /// return a pool connection, and does not grant write admission.
    pub(super) fn configure_standalone_connection(
        &self,
        conn: &Connection,
    ) -> Result<(), SqliteError> {
        #[cfg(feature = "namespace-trigram-proto")]
        register_namespace_trigram(conn)?;
        register_writer_clock(conn)?;
        // Expression indexes over these keys are maintained by every writer
        // (the write-queue task and per-store standalone writers included), so
        // each of them needs the same functions the pooled writer registers.
        register_read_functions(conn)?;
        conn.busy_timeout(self.config.busy_timeout)?;
        if self.config.code_map_vfs.is_none() {
            self.checkpoint_ownership
                .configure_wal_autocheckpoint(conn)?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let wal_enabled =
            self.config.wal_mode && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");
        if wal_enabled {
            conn.pragma_update(
                None,
                "journal_size_limit",
                self.config.journal_size_limit_bytes,
            )?;
        }

        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(conn, &self.statement_observer)?;
        Ok(())
    }

    #[cfg(any(unix, windows))]
    pub(super) fn verify_opened_file_identity(&self, path: &Path) -> Result<(), SqliteError> {
        let Some(expected) = self.opened_file_identity else {
            return Err(SqliteError::InvalidData(
                "file-backed pool has no opened database file identity".to_string(),
            ));
        };
        let current = database_file_identity(path).ok();
        if current != Some(expected) {
            return Err(SqliteError::InvalidData(
                "pool database file identity changed since the first open; refusing standalone connection"
                    .to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(any(unix, windows))]
    pub(super) fn verify_connection_file_identity(
        &self,
        conn: &Connection,
        path: &Path,
    ) -> Result<(), SqliteError> {
        let opened = opened_sqlite_file_identity(conn, path)?;
        if self.opened_file_identity != Some(opened) {
            return Err(SqliteError::InvalidData(
                "pool database file identity changed since the first open; refusing standalone connection"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn verify_opened_database_id(&self, conn: &Connection) -> Result<(), SqliteError> {
        // A read-only pool, or a writable pool opened below the space reserve,
        // may precede installation of the nonce by another process. Without a
        // nonce to pin, use the existing file identity checks where available.
        let Some(expected) = self.opened_database_id else {
            return Ok(());
        };
        if self.identity_path.is_some() && read_database_id(conn)? != Some(expected) {
            return Err(SqliteError::InvalidData(
                "pool database identity changed since the first open; refusing standalone connection"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Effective `PRAGMA wal_autocheckpoint` for a writer-capable connection
    /// opened right now: `0` once a dedicated checkpoint owner has claimed
    /// the pool, the bounded fallback otherwise.
    #[cfg(test)]
    pub(crate) fn effective_wal_autocheckpoint_pages(&self) -> u32 {
        self.checkpoint_ownership.wal_autocheckpoint_pages()
    }

    /// Claim routine WAL-checkpoint ownership for this pool.
    ///
    /// Called by the scheduled checkpoint task at startup — the one caller
    /// that actually replaces SQLite's per-commit autocheckpoint with
    /// dedicated PASSIVE checkpointing (ADR-091 Amendment 10). The claim
    /// makes every subsequently opened writer-capable connection set
    /// `PRAGMA wal_autocheckpoint = 0`, and re-applies that pragma on the
    /// already-open pooled writer under the writer mutex. A writer task
    /// spawned before the claim keeps its own long-lived connection;
    /// [`Self::propagate_checkpoint_claim_to_writer_task`] reaches that one.
    ///
    /// Without a claim, writer-capable connections keep the bounded
    /// `FALLBACK_WAL_AUTOCHECKPOINT_PAGES` threshold, so a writable pool
    /// in a process that never runs the checkpoint task (embedded runtimes,
    /// one-shot CLI executions) retains SQLite's own WAL reclamation instead
    /// of growing its WAL without bound.
    ///
    /// Read-only pools record the claim but have no writer-capable
    /// connections to reconfigure. Writable pools publish the claim only after
    /// the pooled writer is configured successfully; a failed attempt keeps
    /// the bounded fallback active and remains retryable.
    pub fn claim_checkpoint_ownership(&self) -> Result<(), SqliteError> {
        if !self.checkpoint_ownership.begin_claim() {
            return Ok(());
        }
        let result = (|| {
            if !self.config.read_only {
                let writer = self.writer_for_checkpoint_operation()?;
                writer.conn().pragma_update(None, "wal_autocheckpoint", 0)?;
            }
            Ok(())
        })();
        self.checkpoint_ownership.finish_claim(result.is_ok());
        result
    }

    /// Flip an already-running writer task's long-lived connection to the
    /// claimed-owner setting.
    ///
    /// Connections opened after [`Self::claim_checkpoint_ownership`] inherit
    /// `wal_autocheckpoint = 0` at open; only a writer task spawned before
    /// the claim still holds a connection on the bounded fallback. Returns
    /// `Ok(())` without side effects when the pool's write queue is
    /// disabled.
    pub async fn propagate_checkpoint_claim_to_writer_task(&self) -> Result<(), StorageError> {
        let Some(handle) = self.writer_task_handle()? else {
            return Ok(());
        };
        handle
            .send_top_level(|conn| {
                conn.pragma_update(None, "wal_autocheckpoint", 0)
                    .map_err(|e| StorageError::Pool {
                        operation: "claim_checkpoint_ownership".into(),
                        message: e.to_string(),
                    })
            })
            .await
    }

    /// Open a standalone read-only connection for one enumerated structural
    /// exception to pooled-reader routing.
    ///
    /// There is intentionally no public/generic standalone-reader fallback.
    /// Ordinary request reads use [`Self::reader`] and surface bounded pool
    /// exhaustion. Adding a purpose variant or call site is therefore a
    /// deliberate, scoped architecture change (ADR-165 Slice 2), not a
    /// routine extension.
    pub(crate) fn open_standalone_reader(
        &self,
        purpose: StandaloneReaderPurpose,
    ) -> Result<Connection, SqliteError> {
        let path = self.read_connection_path()?;

        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_opened_file_identity(identity_path)?;
        }

        let conn = claimed_file_identity::open_connection(
            &self.config,
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
            self.identity_path.as_deref(),
        )?;
        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_connection_file_identity(&conn, identity_path)?;
        }
        self.verify_opened_database_id(&conn)?;
        configure_reader_connection(&conn, &self.config)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.reader_acquisition_counters
            .record_standalone_open(purpose);
        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(&conn, &self.statement_observer)?;
        Ok(conn)
    }

    pub(super) fn return_reader(&self, conn: Box<PooledReader>, dirty: bool) {
        if self.max_readers == 0 {
            return;
        }

        if !conn.past_limit(&self.config, Instant::now())
            && reset_reader_connection(&conn.conn, dirty, &self.config)
            && reader_connection_is_healthy(&conn.conn)
        {
            self.enqueue_reader_slot(conn);
            return;
        }

        close_connection_quietly(conn.conn);
        self.replace_discarded_reader_slot();
    }

    /// Push a connection back onto the physical reader queue, discarding it
    /// (rather than growing the queue past its configured capacity) if the
    /// queue is already full.
    fn enqueue_reader_slot(&self, conn: Box<PooledReader>) {
        if let Err(conn) = self.readers.push(conn) {
            eprintln!("[sqlite-pool] reader pool queue full, discarding replacement connection");
            close_connection_quietly(conn.conn);
        }
    }

    /// Open a fresh connection to refill one physical reader slot after its
    /// previous occupant was closed and discarded — either disqualified on
    /// an ordinary return ([`Self::return_reader`]) or abandoned by a
    /// non-reusable checkout ([`ReaderGuard`]'s `Drop`). Both call sites
    /// share this so a failed replacement is recorded and logged identically
    /// either way, instead of one path silently shrinking the pool.
    pub(super) fn replace_discarded_reader_slot(&self) {
        self.reader_acquisition_counters
            .reader_discards
            .fetch_add(1, Ordering::Relaxed);
        match self.open_pooled_reader() {
            Ok(conn) => self.enqueue_reader_slot(conn),
            Err(error) => {
                self.reader_acquisition_counters
                    .record_reader_replacement_open_failure();
                tracing::warn!(target: "khive_db::pool",
                    %error,
                    "sqlite-pool: reader replacement connection failed to open; the physical \
                     pool permanently shrinks by one slot below max_readers"
                );
            }
        }
    }
}
