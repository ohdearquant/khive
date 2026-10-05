use super::*;

#[cfg(test)]
pub(super) type SpaceProbe = dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync;

#[cfg(test)]
thread_local! {
    pub(super) static STARTUP_SPACE_PROBE: std::cell::RefCell<Option<(u64, Arc<SpaceProbe>)>> =
        const { std::cell::RefCell::new(None) };
}

/// The SQLite write reserve is sampled at each operation admission. SQLite
/// does not expose the size of an arbitrary upcoming transaction, so the
/// reserve is a warning boundary, not a guarantee that a single very large
/// transaction cannot consume more than the remaining headroom.
pub(crate) struct WriteAdmission {
    database_path: Option<PathBuf>,
    volume: Option<PathBuf>,
    expected_volume: Option<VolumeIdentity>,
    volume_lock_dir: Option<PathBuf>,
    floor_bytes: u64,
    guard_deadline: Duration,
    #[cfg(test)]
    space_probe: Mutex<Option<Arc<SpaceProbe>>>,
    #[cfg(test)]
    current_volume_override: Mutex<Option<VolumeIdentity>>,
}

impl WriteAdmission {
    pub(super) fn new(
        database_path: Option<PathBuf>,
        floor_bytes: u64,
        guard_deadline_ms: u64,
        volume_lock_dir: Option<PathBuf>,
    ) -> Result<Self, SqliteError> {
        let volume = database_path.clone();
        let expected_volume = volume.as_deref().map(VolumeIdentity::resolve).transpose()?;
        #[cfg(test)]
        let (floor_bytes, space_probe) = STARTUP_SPACE_PROBE.with(|probe| {
            probe
                .borrow()
                .as_ref()
                .map(|(floor, probe)| (*floor, Some(Arc::clone(probe))))
                .unwrap_or((floor_bytes, None))
        });
        Ok(Self {
            database_path,
            volume,
            expected_volume,
            volume_lock_dir,
            floor_bytes,
            guard_deadline: Duration::from_millis(guard_deadline_ms),
            #[cfg(test)]
            space_probe: Mutex::new(space_probe),
            #[cfg(test)]
            current_volume_override: Mutex::new(None),
        })
    }

    /// Build admission for a raw connection with no backend-local override.
    /// Pooled callers must use their pool's effective policy instead.
    pub(crate) fn for_canonical_path(database_path: Option<PathBuf>) -> Result<Self, SqliteError> {
        if database_path.is_none() {
            return Self::new(None, 0, DEFAULT_DISK_GUARD_DEADLINE_MS, None);
        }
        let policy = crate::migrations::MigrationWritePolicy::from_environment()?;
        Self::for_migration_policy(database_path, &policy)
    }

    pub(crate) fn for_migration_policy(
        database_path: Option<PathBuf>,
        policy: &crate::migrations::MigrationWritePolicy,
    ) -> Result<Self, SqliteError> {
        let effective = policy.disk_guard_config();
        if effective.legacy_environment_present {
            tracing::warn!(
                "legacy SQLite reserve setting is deprecated; use KHIVE_SQLITE_DISK_RESERVE_BYTES"
            );
        }
        if effective.reserve_bytes == 0 {
            tracing::warn!(
                "SQLite disk reserve is explicitly zero; new logical writes will not be floor-refused"
            );
        }
        Self::new(
            database_path,
            effective.reserve_bytes,
            effective.guard_deadline_ms,
            Some(policy.volume_lock_dir().to_path_buf()),
        )
    }

    pub(super) fn verify_current_volume(&self) -> Result<(), SqliteError> {
        let (Some(path), Some(expected)) = (self.volume.as_deref(), self.expected_volume.as_ref())
        else {
            return Ok(());
        };
        #[cfg(test)]
        let current = self.current_volume_override.lock().clone();
        #[cfg(not(test))]
        let current: Option<VolumeIdentity> = None;
        let current = match current {
            Some(identity) => identity,
            None => VolumeIdentity::resolve(path)?,
        };
        if &current != expected {
            return Err(SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Identity,
                message: "database volume changed after admission identity was captured"
                    .to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn close_retired_connection(&self, conn: Connection) {
        let database_path = self
            .database_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| ":memory:".to_string());
        let volume_key = self
            .expected_volume
            .as_ref()
            .map(VolumeIdentity::diagnostic_key)
            .unwrap_or_else(|| "in-memory".to_string());
        crate::connection_settlement::close_retired_connection(conn, &database_path, &volume_key);
    }

    pub(crate) fn volume_identity(&self) -> Result<Option<VolumeIdentity>, SqliteError> {
        self.verify_current_volume()?;
        Ok(self.expected_volume.clone())
    }

    fn available_space(&self, volume: &Path) -> std::io::Result<u64> {
        #[cfg(test)]
        if let Some(probe) = self.space_probe.lock().as_ref() {
            return probe(volume);
        }
        fs4::available_space(volume)
    }

    pub(crate) fn check(&self) -> Result<(), SqliteError> {
        self.check_with_headroom(0)
    }

    pub(crate) fn check_with_headroom(
        &self,
        required_headroom_bytes: u64,
    ) -> Result<(), SqliteError> {
        let Some(volume) = self.volume.as_deref() else {
            return Ok(());
        };
        self.verify_current_volume()?;
        let identity =
            self.expected_volume
                .as_ref()
                .ok_or_else(|| SqliteError::CapacityUnavailable {
                    phase: CapacityUnavailablePhase::Identity,
                    message: "file-backed admission has no captured volume identity".to_string(),
                })?;
        let available = self
            .available_space(identity.probe_path())
            .map_err(|error| SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Probe,
                message: format!(
                    "cannot sample available space on {}: {error}",
                    volume.display()
                ),
            })?;
        self.verify_current_volume()?;
        // SQL does not tell admission how many bytes a generic transaction
        // will append. VACUUM supplies its known copy-sized estimate here.
        // Overflow is a refusal, never a wrapped low threshold.
        let threshold = self.floor_bytes.checked_add(required_headroom_bytes);
        if threshold.is_none()
            || (threshold != Some(0) && available <= threshold.unwrap_or(u64::MAX))
        {
            return Err(SqliteError::CapacityFloor {
                volume: volume.display().to_string(),
                available_bytes: available,
                floor_bytes: self.floor_bytes,
                required_headroom_bytes,
            });
        }
        Ok(())
    }

    /// Acquire the physical-volume lease before SQLite writer acquisition.
    /// The caller must retain the returned guard through settlement.
    pub(crate) fn acquire(&self) -> Result<Option<VolumeLease>, SqliteError> {
        let Some(identity) = self.expected_volume.as_ref() else {
            return Ok(None);
        };
        self.verify_current_volume()?;
        let lease = identity.acquire(self.guard_deadline, self.volume_lock_dir.as_deref())?;
        self.verify_current_volume()?;
        Ok(Some(lease))
    }

    pub(crate) fn vacuum_headroom(&self) -> Result<u64, SqliteError> {
        self.database_path
            .as_deref()
            .map(crate::vacuum_capacity::estimate_vacuum_headroom)
            .transpose()
            .map(|headroom| headroom.unwrap_or(0))
    }

    #[cfg(test)]
    pub(super) fn set_test_space_probe(
        &self,
        probe: impl Fn(&Path) -> std::io::Result<u64> + Send + Sync + 'static,
    ) {
        *self.space_probe.lock() = Some(Arc::new(probe));
    }

    #[cfg(test)]
    pub(crate) fn set_test_current_volume(&self, identity: Option<VolumeIdentity>) {
        *self.current_volume_override.lock() = identity;
    }

    #[cfg(test)]
    pub(crate) fn captured_volume_for_test(&self) -> Option<VolumeIdentity> {
        self.expected_volume.clone()
    }
}

/// A writer connection checked out from the pool.
/// The Mutex ensures only one writer at a time.
///
/// # Aborts
/// Dropping a retired writer aborts if cleanup cannot restore autocommit and
/// owned SQLite close also fails. Database path, volume, and errors are emitted
/// to stderr and tracing before aborting; the lease is never released early.
pub struct WriterGuard<'pool> {
    pub(super) guard: parking_lot::MutexGuard<'pool, Connection>,
    /// The origin (ADR-091 backend-scoped attribution) of the pool this
    /// guard was checked out from, carried so `transaction` can register its
    /// span with the correct origin without holding a `&ConnectionPool`.
    pub(super) origin: TxOrigin,
    pub(super) pool: &'pool ConnectionPool,
    pub(super) admission: &'pool WriteAdmission,
    /// Normal writer checkout acquires this before taking the writer mutex.
    /// Maintenance-only nowait checkout leaves it absent to bypass the floor.
    pub(super) _volume_lease: Option<VolumeLease>,
}

/// One synchronous pooled autocommit write or script. Construction samples
/// capacity under the volume lease, and the owned writer guard retains that
/// lease until this unit is dropped after SQLite returns to autocommit.
pub(crate) struct PooledAutocommitWriteUnit<'pool> {
    writer: WriterGuard<'pool>,
}

impl PooledAutocommitWriteUnit<'_> {
    pub(crate) fn conn(&self) -> &Connection {
        self.writer.conn()
    }
}

/// One pooled transaction after lease, BEGIN, and capacity admission. The
/// owned writer guard rolls back on drop if the caller fails to settle it.
pub(crate) struct PooledTransactionWriteUnit<'pool> {
    writer: WriterGuard<'pool>,
}

impl PooledTransactionWriteUnit<'_> {
    pub(crate) fn conn(&self) -> &Connection {
        self.writer.conn()
    }
}

/// One standalone transaction on an owned connection. Settlement (autocommit
/// or successful owned close) precedes release of the volume lease, including
/// when the operation unwinds or its initial rollback fails.
pub(crate) struct StandaloneTransactionWriteUnit {
    conn: Option<Connection>,
    admission: Arc<WriteAdmission>,
    _volume_lease: Option<VolumeLease>,
}

impl StandaloneTransactionWriteUnit {
    pub(crate) fn conn(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("standalone write unit owns its connection")
    }
}

impl Drop for StandaloneTransactionWriteUnit {
    fn drop(&mut self) {
        let conn = self
            .conn
            .take()
            .expect("standalone write unit retirement owner");
        if conn.is_autocommit() {
            drop(conn);
        } else {
            self.admission.close_retired_connection(conn);
        }
    }
}

/// A zero-wait checkout that can run only the fixed checkpoint recovery
/// pragmas. The connection remains private: exposing it would let a caller
/// execute logical writes without disk-reserve admission (ADR-154 §5).
///
/// Ordinary SQL is deliberately unavailable through this capability:
/// ```compile_fail
/// use khive_db::{ConnectionPool, PoolConfig};
/// let pool = ConnectionPool::new(PoolConfig::default()).unwrap();
/// pool.try_checkpoint_nowait().unwrap().execute_batch("CREATE TABLE bypass (id INTEGER)");
/// ```
pub struct CheckpointGuard<'pool> {
    pub(super) guard: parking_lot::MutexGuard<'pool, Connection>,
}

/// SQLite's three-column result from a fixed WAL checkpoint pragma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointResult {
    /// Whether SQLite reported a busy checkpoint.
    pub busy: i64,
    /// WAL frames observed by SQLite (`-1` when there is no WAL).
    pub log_frames: i64,
    /// WAL frames copied back into the database.
    pub checkpointed_frames: i64,
}

impl CheckpointGuard<'_> {
    /// Run a PASSIVE checkpoint without disk-reserve admission.
    pub fn passive(&self) -> Result<CheckpointResult, SqliteError> {
        self.guard
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok(CheckpointResult {
                    busy: row.get(0)?,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
            .map_err(Into::into)
    }

    /// Run a TRUNCATE checkpoint without disk-reserve admission.
    pub fn truncate(&self) -> Result<CheckpointResult, SqliteError> {
        self.guard
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok(CheckpointResult {
                    busy: row.get(0)?,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
            .map_err(Into::into)
    }
}

/// Process-local monotonic counters for every instrumented writer acquisition
/// boundary owned by one [`ConnectionPool`].
///
/// The aggregate `acquisitions` is the saturating sum of its three explicit
/// connection classes. Infrastructure-only opens (the diagnostics PASSIVE
/// probe, the writer task's one-time lifetime connection, and the checkpoint
/// task's dedicated long-lived connection) are excluded; zero-wait
/// maintenance probes also remain outside these request-traffic counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterAcquisitionSnapshot {
    /// Successful acquisitions across pooled, standalone, and writer-task
    /// connection classes.
    pub acquisitions: u64,
    /// Successful finite-wait pool-mutex writer checkouts.
    pub pooled_acquisitions: u64,
    /// Successful per-operation standalone writer connection opens.
    pub standalone_acquisitions: u64,
    /// Successful writer-task ownership acquisitions (one per dequeued
    /// top-level request or successful `BEGIN IMMEDIATE`).
    pub writer_task_acquisitions: u64,
    /// Finite-wait pool writer checkouts that exhausted their deadline.
    pub timeouts: u64,
    /// Instrumented direct executions whose final returned error retains SQLite's
    /// primary DatabaseBusy code, once per operation after its busy handler.
    /// Excludes LOCKED, checkout/open/admission failures, readers, writer tasks,
    /// infrastructure probes and uninstrumented raw connection escapes.
    pub direct_busy_refusals: u64,
    /// Every writer-task `BEGIN IMMEDIATE` attempt refused busy or locked,
    /// including refusals a subsequent bounded retry went on to absorb.
    /// Counted separately from `timeouts` because that counter names the
    /// pool-mutex checkout stage; folding the two would mislabel the stage.
    pub writer_task_begin_busy: u64,
    /// Subset of `writer_task_begin_busy` that a subsequent bounded retry
    /// absorbed before the request closure ran, so the refusal never
    /// reached the caller. `writer_task_begin_busy - writer_task_begin_busy_absorbed`
    /// is the count of refusals a caller actually observed.
    pub writer_task_begin_busy_absorbed: u64,
    /// Writer-task `BEGIN IMMEDIATE` attempts that failed for a reason other
    /// than busy or locked, and so surface as `StorageError::Pool`.
    pub writer_task_begin_errors: u64,
    /// Dequeued writer-task requests that reached the writer seam (executed
    /// or attempted to execute their operation) and terminated in error,
    /// counted once per request regardless of the specific terminal state.
    pub writer_task_request_failures: u64,
    /// Subset of `writer_task_request_failures` whose terminal state was
    /// `WriterTaskRequestState::SideEffectsUnknown` — the commit or rollback
    /// outcome could not be established, so the request's side effects on
    /// the database are unknown.
    pub writer_task_side_effects_unknown: u64,
}

/// Atomics backing [`WriterAcquisitionSnapshot`]. The writer task retains an
/// `Arc` after spawn so its per-request acquisition site can update the same
/// pool-scoped snapshot without retaining the whole pool.
#[derive(Debug, Default)]
pub(crate) struct WriterAcquisitionCounters {
    pub(super) pooled_acquisitions: AtomicU64,
    pub(super) standalone_acquisitions: AtomicU64,
    pub(super) writer_task_acquisitions: AtomicU64,
    pub(super) pooled_timeouts: AtomicU64,
    pub(super) direct_busy_refusals: AtomicU64,
    pub(super) writer_task_begin_busy: AtomicU64,
    pub(super) writer_task_begin_busy_absorbed: AtomicU64,
    pub(super) writer_task_begin_errors: AtomicU64,
    pub(super) writer_task_request_failures: AtomicU64,
    pub(super) writer_task_side_effects_unknown: AtomicU64,
}

impl<'pool> WriterGuard<'pool> {
    /// Bind the lease already owned by this guard to one autocommit write
    /// unit. The caller must construct the unit immediately before its first
    /// SQLite write, then keep it through the last call in that unit.
    pub(crate) fn admit_autocommit(self) -> Result<PooledAutocommitWriteUnit<'pool>, SqliteError> {
        if !self.guard.is_autocommit() {
            self.pool.retire_pooled_writer(&self.guard);
            return Err(SqliteError::InvalidData(
                "pooled autocommit write began on a connection in a transaction".to_string(),
            ));
        }
        if self._volume_lease.is_none() && self.pool.canonical_path().is_some() {
            return Err(SqliteError::InvalidData(
                "checkpoint writer checkout cannot admit a logical write".to_string(),
            ));
        }
        self.admission.check()?;
        Ok(PooledAutocommitWriteUnit { writer: self })
    }

    /// Returns a shared reference to the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.guard
    }

    /// Returns a mutable reference to the underlying connection.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }

    /// Execute a write transaction.
    /// Wraps the closure in BEGIN IMMEDIATE ... COMMIT.
    pub fn transaction<F, R>(&self, f: F) -> Result<R, SqliteError>
    where
        F: FnOnce(&Connection) -> Result<R, SqliteError>,
    {
        if self._volume_lease.is_none() && self.pool.canonical_path().is_some() {
            return Err(SqliteError::InvalidData(
                "checkpoint writer checkout cannot start a logical transaction".to_string(),
            ));
        }
        self.guard.execute_batch("BEGIN IMMEDIATE")?;
        if let Err(error) = self.admission.check() {
            self.rollback_or_retire("capacity admission")?;
            return Err(error);
        }
        let _tx_handle = khive_storage::tx_registry::register_scoped(
            Some("writer_guard_tx".to_string()),
            self.origin.clone(),
        );

        match f(&self.guard) {
            Ok(result) => {
                if let Err(err) = self.guard.execute_batch("COMMIT") {
                    self.rollback_or_retire("commit failure")?;
                    return Err(err.into());
                }
                Ok(result)
            }
            Err(err) => {
                self.rollback_or_retire("transaction body failure")?;
                Err(err)
            }
        }
    }

    fn rollback_or_retire(&self, context: &str) -> Result<(), SqliteError> {
        let rollback = self.guard.execute_batch("ROLLBACK");
        if rollback.is_err() || !self.guard.is_autocommit() {
            self.pool.retire_pooled_writer(&self.guard);
            tracing::error!(context, "pooled writer rollback did not prove autocommit");
            return Err(SqliteError::WriterSettlementUnknown);
        }
        Ok(())
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        if !self.guard.is_autocommit() {
            let _ = self.rollback_or_retire("guard dropped with open transaction");
        }
        if self.pool.pooled_writer_retired.load(Ordering::Acquire) {
            if let Some(replacement) = self.pool.retirement_connection.lock().take() {
                // The mutex excludes all aliases; settlement precedes lease release.
                let original = std::mem::replace(&mut *self.guard, replacement);
                self.admission.close_retired_connection(original);
            }
        }
    }
}

impl<'pool> Deref for WriterGuard<'pool> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn()
    }
}

impl<'pool> DerefMut for WriterGuard<'pool> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn_mut()
    }
}

impl ConnectionPool {
    pub(crate) fn autocommit_write_unit(
        &self,
    ) -> Result<PooledAutocommitWriteUnit<'_>, SqliteError> {
        self.writer_for_admitted_operation()?.admit_autocommit()
    }

    pub(crate) fn transaction_write_unit(
        &self,
    ) -> Result<super::PooledTransactionWriteUnit<'_>, SqliteError> {
        let writer = self.writer_for_admitted_operation()?;
        if !writer.guard.is_autocommit() {
            writer.pool.retire_pooled_writer(&writer.guard);
            return Err(SqliteError::InheritedWriterTransaction);
        }
        if let Err(error) = writer.guard.execute_batch("BEGIN IMMEDIATE") {
            if !writer.guard.is_autocommit() {
                writer.pool.retire_pooled_writer(&writer.guard);
                return Err(SqliteError::WriterSettlementUnknown);
            }
            return Err(error.into());
        }
        if let Err(error) = writer.admission.check() {
            writer.rollback_or_retire("capacity admission")?;
            return Err(error);
        }
        Ok(PooledTransactionWriteUnit { writer })
    }

    /// Execute DML on a typed direct transaction, choosing the standalone
    /// connection required by file-backed async stores or the pooled writer
    /// used by in-memory stores. The callback cannot begin or commit its own
    /// outer transaction; this seam owns settlement and the volume lease.
    pub(crate) fn execute_direct_transaction<R, F>(
        &self,
        capability: StorageCapability,
        operation: &'static str,
        f: F,
    ) -> Result<R, StorageError>
    where
        F: FnOnce(&Connection) -> Result<R, StorageError>,
    {
        let _tx_handle =
            khive_storage::tx_registry::register_scoped(Some(operation.to_string()), self.origin());
        let db_label = crate::timeout_sink::db_label(self);
        let map_admission_error = |error: SqliteError| {
            crate::timeout_sink::maybe_emit_sqlite_full(&db_label, &error);
            let error = error.into_storage_error(capability, operation);
            self.record_direct_writer_error(&error);
            error
        };
        let result = if self.canonical_path().is_some() {
            let unit = self
                .standalone_transaction_write_unit()
                .map_err(map_admission_error)?;
            let (result, _) = execute_wrapped_transaction(unit.conn(), operation, f);
            result
        } else {
            let unit = self.transaction_write_unit().map_err(map_admission_error)?;
            let conn = unit.conn();
            let (result, terminal_state) = execute_wrapped_transaction(conn, operation, f);
            if terminal_state.is_some() {
                self.retire_pooled_writer(conn);
            }
            result
        };
        if let Err(error) = &result {
            crate::timeout_sink::maybe_emit_sqlite_full(&db_label, error);
            self.record_direct_writer_error(error);
        }
        result
    }

    /// Run core migrations through the owner-before-volume-before-writer
    /// order for callers that hold a pool but not a `StorageBackend` wrapper.
    /// Passing a pooled raw `&mut Connection` to the public migration API
    /// would reacquire this same volume lease and can self-deadlock.
    pub fn run_migrations(&self) -> Result<u32, SqliteError> {
        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "cannot run migrations on a read-only pool".to_string(),
            ));
        }
        let owner = crate::stores::blob::acquire_database_gc_owner_for_path_blocking(
            self.canonical_path().map(Path::to_path_buf),
        )
        .map_err(|error| {
            SqliteError::InvalidData(format!(
                "failed to acquire database GC owner before schema migration: {error}"
            ))
        })?;
        let mut writer = self.writer_for_admitted_operation()?;
        crate::migrations::run_migrations_with_database_gc_owner(
            writer.conn_mut(),
            &owner,
            &self.write_admission,
        )
    }

    /// Own one standalone write transaction from lease acquisition through
    /// settlement. The free-space sample occurs only after BEGIN IMMEDIATE
    /// has acquired SQLite's writer slot.
    pub(crate) fn standalone_transaction_write_unit(
        &self,
    ) -> Result<super::StandaloneTransactionWriteUnit, SqliteError> {
        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "database is read-only: standalone write transactions are not permitted".into(),
            ));
        }
        let volume_lease = self.write_admission.acquire()?;
        let conn = self.open_standalone_writer_for_admitted_operation()?;
        let unit = StandaloneTransactionWriteUnit {
            conn: Some(conn),
            admission: Arc::clone(&self.write_admission),
            _volume_lease: volume_lease,
        };
        if !unit.conn().is_autocommit() {
            return Err(SqliteError::WriterSettlementUnknown);
        }
        if let Err(error) = unit.conn().execute_batch("BEGIN IMMEDIATE") {
            if !unit.conn().is_autocommit() {
                return Err(SqliteError::WriterSettlementUnknown);
            }
            return Err(error.into());
        }
        if let Err(error) = self.write_admission.check() {
            if unit.conn().execute_batch("ROLLBACK").is_err() || !unit.conn().is_autocommit() {
                return Err(SqliteError::WriterSettlementUnknown);
            }
            return Err(error);
        }
        Ok(unit)
    }
}
