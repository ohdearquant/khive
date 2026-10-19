//! Reader and writer acquisition with bounded admission.

use std::cell::Cell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use khive_storage::error::StorageError;
use khive_storage::StorageCapability;
use rusqlite::Connection;

use crate::error::SqliteError;

use super::{
    deny_retired_writer, pool_exhausted_error, CheckpointGuard, ConnectionPool, ReaderAdmission,
    ReaderGuard, ReaderLease, WriterGuard,
};

impl ConnectionPool {
    /// Check out a reader connection.
    ///
    /// Tries to pop from the lock-free queue. If empty, spins briefly then
    /// waits with exponential backoff up to `checkout_timeout`.
    ///
    /// In degraded mode (WAL unavailable, `max_readers == 0`), this method
    /// checks the shared writer mutex in bounded slices and returns pool
    /// exhaustion after `checkout_timeout`; it never blocks indefinitely on
    /// the non-reentrant mutex.
    pub fn reader(&self) -> Result<ReaderGuard<'_>, SqliteError> {
        self.reader_until(|| false)?.ok_or_else(|| {
            SqliteError::InvalidData("uncancelled reader checkout stopped unexpectedly".into())
        })
    }

    /// Check out a reader while cooperatively polling a request cancellation
    /// predicate. The predicate is evaluated before connection acquisition and
    /// between backoff slices, so an abandoned request does not sit through the
    /// full pool checkout timeout or execute a statement when a reader later
    /// becomes available.
    pub(crate) fn reader_until<C>(
        &self,
        should_stop: C,
    ) -> Result<Option<ReaderGuard<'_>>, SqliteError>
    where
        C: Fn() -> bool,
    {
        let started = Instant::now();
        let mut admission_attempt = 0u32;
        let admission_slot = loop {
            if should_stop() {
                return Ok(None);
            }
            match Arc::clone(&self.sql_bridge_reader_slots).try_acquire_owned() {
                Ok(slot) => break slot,
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(SqliteError::InvalidData(
                        "reader admission semaphore is closed".to_string(),
                    ));
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {}
            }
            if started.elapsed() >= self.config.checkout_timeout {
                self.reader_acquisition_counters.record_checkout_timeout();
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }
            match admission_attempt {
                0..=7 => {
                    let spins = 1usize << admission_attempt;
                    for _ in 0..spins {
                        std::hint::spin_loop();
                    }
                }
                8..=15 => thread::yield_now(),
                _ => {
                    let remaining = self
                        .config
                        .checkout_timeout
                        .saturating_sub(started.elapsed());
                    let sleep =
                        Duration::from_micros(50 * (1u64 << (admission_attempt - 16).min(6)));
                    thread::sleep(sleep.min(remaining).min(Duration::from_millis(2)));
                }
            }
            admission_attempt = admission_attempt.saturating_add(1);
        };

        self.reader_with_admission(
            ReaderAdmission {
                slot: admission_slot,
                started,
            },
            should_stop,
        )
    }

    /// Wait for one pool-wide reader admission permit on the async side, so a
    /// read that has to queue costs a task and not a blocking-pool thread.
    ///
    /// The wait is bounded by `checkout_timeout` and by the current request's
    /// cancellation and deadline, and it yields the same `Ok(Some)` / `Ok(None)`
    /// / `Err` outcomes the permit loop in [`Self::reader_until`] does, resolved
    /// through the same refusal mapping as [`Self::resolve_reader_checkout`].
    /// Dropping the future abandons the wait without taking a permit.
    pub(crate) async fn acquire_reader_admission(
        &self,
        capability: StorageCapability,
        operation: &'static str,
    ) -> Result<ReaderAdmission, StorageError> {
        let context = khive_storage::capture_request_read_context();
        let started = Instant::now();
        let stopped = context.stop_reason().is_some();
        let outcome: Result<Option<ReaderAdmission>, SqliteError> = if stopped {
            Ok(None)
        } else {
            tokio::select! {
                biased;
                _ = context.wait_for_stop() => Ok(None),
                waited = tokio::time::timeout(
                    self.config.checkout_timeout,
                    Arc::clone(&self.sql_bridge_reader_slots).acquire_owned(),
                ) => match waited {
                    Ok(Ok(slot)) => Ok(Some(ReaderAdmission { slot, started })),
                    Ok(Err(_closed)) => Err(SqliteError::InvalidData(
                        "reader admission semaphore is closed".to_string(),
                    )),
                    Err(_elapsed) => {
                        self.reader_acquisition_counters.record_checkout_timeout();
                        Err(pool_exhausted_error(
                            self.config.checkout_timeout,
                            self.max_readers,
                        ))
                    }
                },
            }
        };
        match outcome {
            Ok(Some(admission)) => Ok(admission),
            Ok(None) => Err(self.reader_checkout_refusal(capability, operation, None)),
            Err(error) => Err(self.reader_checkout_refusal(capability, operation, Some(error))),
        }
    }

    /// Select the reader connection for an admission permit already held.
    ///
    /// This is [`Self::reader_until`] after its permit loop: the same
    /// `should_stop` polling, the same `Ok(Some)` / `Ok(None)` / `Err`
    /// outcomes, and the remainder of the one `checkout_timeout` that began
    /// when the permit wait did.
    pub(crate) fn reader_with_admission<C>(
        &self,
        admission: ReaderAdmission,
        should_stop: C,
    ) -> Result<Option<ReaderGuard<'_>>, SqliteError>
    where
        C: Fn() -> bool,
    {
        let ReaderAdmission {
            slot: admission_slot,
            started,
        } = admission;

        if self.max_readers == 0 {
            self.ensure_pooled_writer_active()?;
            loop {
                if should_stop() {
                    return Ok(None);
                }
                let remaining = self
                    .config
                    .checkout_timeout
                    .saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    self.reader_acquisition_counters.record_checkout_timeout();
                    return Err(pool_exhausted_error(
                        self.config.checkout_timeout,
                        self.max_readers,
                    ));
                }
                if let Some(guard) = self
                    .writer
                    .try_lock_for(remaining.min(Duration::from_millis(2)))
                {
                    self.ensure_pooled_writer_active()?;
                    self.reader_acquisition_counters.record_pooled_checkout();
                    return Ok(Some(ReaderGuard {
                        lease: Some(ReaderLease::Shared(guard)),
                        admission_slot: Some(admission_slot),
                        pool: self,
                        reusable: Cell::new(true),
                        query_in_progress: Cell::new(false),
                        checked_out_at: self.begin_reader_checkout(),
                        dirty: Cell::new(false),
                        operation: None,
                    }));
                }
            }
        }

        let mut attempt = 0u32;

        loop {
            if should_stop() {
                return Ok(None);
            }
            if let Some(mut conn) = self.readers.pop() {
                conn.checkouts = conn.checkouts.saturating_add(1);
                self.reader_acquisition_counters.record_pooled_checkout();
                return Ok(Some(ReaderGuard {
                    lease: Some(ReaderLease::Pooled(conn)),
                    admission_slot: Some(admission_slot),
                    pool: self,
                    reusable: Cell::new(true),
                    query_in_progress: Cell::new(false),
                    checked_out_at: self.begin_reader_checkout(),
                    dirty: Cell::new(false),
                    operation: None,
                }));
            }

            if started.elapsed() >= self.config.checkout_timeout {
                self.reader_acquisition_counters.record_checkout_timeout();
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }

            match attempt {
                0..=7 => {
                    let spins = 1usize << attempt;
                    for _ in 0..spins {
                        std::hint::spin_loop();
                    }
                }
                8..=15 => thread::yield_now(),
                _ => {
                    let remaining = self
                        .config
                        .checkout_timeout
                        .saturating_sub(started.elapsed());
                    let sleep = Duration::from_micros(50 * (1u64 << (attempt - 16).min(6)));
                    thread::sleep(sleep.min(remaining).min(Duration::from_millis(2)));
                }
            }

            attempt = attempt.saturating_add(1);
        }
    }

    /// Check out the writer connection.
    ///
    /// Waits up to `checkout_timeout` for the writer Mutex and returns
    /// `Err(SqliteError::WriterPoolCheckoutTimeout)` if the timeout is
    /// exceeded.
    ///
    /// Checkout owns exclusion, not a capacity sample. Consume the returned
    /// guard with [`WriterGuard::autocommit`] or [`WriterGuard::transaction`];
    /// those operations sample at execution and retain ownership through
    /// settlement. Raw connection access is crate-private.
    #[track_caller]
    pub fn writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
        self.writer_checkout(false)
    }

    /// Callers that own an explicit §4 logical-write boundary probe there,
    /// so checkout must not refuse ahead of their BEGIN/bootstrap statement.
    #[track_caller]
    pub(crate) fn writer_for_admitted_operation(&self) -> Result<WriterGuard<'_>, SqliteError> {
        self.writer_checkout(false)
    }

    /// Checkpoint ownership setup changes only SQLite's checkpoint policy.
    /// Its bounded checkout must remain available below the write reserve.
    pub(super) fn writer_for_checkpoint_operation(&self) -> Result<WriterGuard<'_>, SqliteError> {
        self.writer_checkout(true)
    }

    #[track_caller]
    fn writer_checkout(&self, checkpoint_bypass: bool) -> Result<WriterGuard<'_>, SqliteError> {
        if !checkpoint_bypass {
            self.write_admission.ensure_settled()?;
        }
        self.ensure_pooled_writer_active()?;
        let volume_lease = if checkpoint_bypass {
            None
        } else {
            self.write_admission.acquire()?
        };
        let Some(guard) = self.writer.try_lock_for(self.config.checkout_timeout) else {
            self.writer_acquisition_counters
                .pooled_timeouts
                .fetch_add(1, Ordering::Relaxed);
            let message = format!(
                "timed out after {:?} waiting for sqlite writer connection",
                self.config.checkout_timeout
            );
            crate::timeout_sink::emit_timeout(
                &crate::timeout_sink::db_label(self),
                crate::timeout_sink::Site::PoolAdmission,
                &message,
                Some(
                    self.config
                        .checkout_timeout
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                ),
            );
            return Err(SqliteError::WriterPoolCheckoutTimeout {
                timeout: self.config.checkout_timeout,
            });
        };
        self.ensure_pooled_writer_active()?;
        #[cfg(any(unix, windows))]
        if let Some(path) = self.identity_path.as_deref() {
            self.verify_opened_file_identity(path)?;
            self.verify_connection_file_identity(&guard, path)?;
        }
        self.writer_acquisition_counters
            .pooled_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        Ok(WriterGuard {
            guard,
            origin: self.origin(),
            pool: self,
            admission: self.write_admission.as_ref(),
            _volume_lease: volume_lease,
        })
    }

    /// Non-panicking writer checkout.
    ///
    /// Returns `Err` on timeout instead of panicking. Use this in request
    /// handlers where a 500 is preferable to crashing the process.
    #[track_caller]
    pub fn try_writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
        self.writer()
    }

    #[track_caller]
    pub(crate) fn writer_until_for_admitted_operation<C>(
        &self,
        should_stop: C,
    ) -> Result<Option<WriterGuard<'_>>, SqliteError>
    where
        C: Fn() -> bool,
    {
        self.ensure_pooled_writer_active()?;
        if should_stop() {
            return Ok(None);
        }
        let volume_lease = self.write_admission.acquire()?;
        let started = Instant::now();
        loop {
            if should_stop() {
                return Ok(None);
            }
            let remaining = self
                .config
                .checkout_timeout
                .saturating_sub(started.elapsed());
            if let Some(guard) = self
                .writer
                .try_lock_for(remaining.min(Duration::from_millis(2)))
            {
                // Cancellation may have arrived during the final wait slice.
                // Once this guard is returned, constructor DDL is not interrupted.
                if should_stop() {
                    return Ok(None);
                }
                self.ensure_pooled_writer_active()?;
                #[cfg(any(unix, windows))]
                if let Some(path) = self.identity_path.as_deref() {
                    self.verify_opened_file_identity(path)?;
                    self.verify_connection_file_identity(&guard, path)?;
                }
                self.writer_acquisition_counters
                    .pooled_acquisitions
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(Some(WriterGuard {
                    guard,
                    origin: self.origin(),
                    pool: self,
                    admission: self.write_admission.as_ref(),
                    _volume_lease: volume_lease,
                }));
            }
            if started.elapsed() >= self.config.checkout_timeout {
                self.writer_acquisition_counters
                    .pooled_timeouts
                    .fetch_add(1, Ordering::Relaxed);
                let message = format!(
                    "timed out after {:?} waiting for sqlite writer connection",
                    self.config.checkout_timeout
                );
                crate::timeout_sink::emit_timeout(
                    &crate::timeout_sink::db_label(self),
                    crate::timeout_sink::Site::PoolAdmission,
                    &message,
                    Some(
                        self.config
                            .checkout_timeout
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    ),
                );
                return Err(SqliteError::WriterPoolCheckoutTimeout {
                    timeout: self.config.checkout_timeout,
                });
            }
        }
    }

    /// Zero-wait checkpoint checkout for recovery maintenance.
    ///
    /// Uses `try_lock()` (no timeout, no spin) — returns `Err` immediately when
    /// any other caller holds the writer Mutex. The scheduled checkpoint task
    /// uses its own dedicated connection (ADR-091 Amendment 5); this optional
    /// pooled capability remains available for zero-wait recovery callers.
    ///
    /// It bypasses the disk-reserve floor because checkpoints can recover WAL
    /// space at or below that floor (ADR-154 §5). The returned guard exposes
    /// only fixed PASSIVE and TRUNCATE checkpoint operations.
    ///
    /// The former unrestricted checkout must not return:
    /// ```compile_fail
    /// use khive_db::{ConnectionPool, PoolConfig};
    /// let pool = ConnectionPool::new(PoolConfig::default()).unwrap();
    /// pool.try_writer_nowait().unwrap().execute_batch("CREATE TABLE bypass (id INTEGER)");
    /// ```
    pub fn try_checkpoint_nowait(&self) -> Result<CheckpointGuard<'_>, SqliteError> {
        self.ensure_pooled_writer_active()?;
        let guard = self.writer.try_lock().ok_or_else(|| {
            SqliteError::InvalidData(
                "writer connection busy (checkpoint skipped this tick)".to_string(),
            )
        })?;
        self.ensure_pooled_writer_active()?;
        Ok(CheckpointGuard { guard })
    }

    pub(crate) fn retire_pooled_writer(&self, conn: &Connection) {
        self.pooled_writer_retired.store(true, Ordering::Release);
        if let Err(error) = conn.authorizer(Some(deny_retired_writer)) {
            tracing::error!(target: "khive_db::pool",
                %error,
                "failed to install the retired pooled-writer quarantine authorizer"
            );
        }
    }

    /// Exercise the retired connection's authorizer without exporting a raw
    /// handle from the pool. This is intentionally unavailable in production.
    #[cfg(test)]
    pub(crate) fn probe_retired_pooled_writer_for_test(&self) -> rusqlite::Result<i64> {
        self.writer
            .lock()
            .query_row("SELECT 1", [], |row| row.get(0))
    }

    /// Leave an inherited transaction on the bare pooled connection to test
    /// the typed write-unit refusal. A normal WriterGuard rolls it back on
    /// drop, so it cannot create this poisoned state.
    #[cfg(test)]
    pub(crate) fn leave_pooled_writer_transaction_open_for_test(&self) -> rusqlite::Result<()> {
        self.writer.lock().execute_batch("BEGIN IMMEDIATE")
    }

    pub(super) fn ensure_pooled_writer_active(&self) -> Result<(), SqliteError> {
        if self.pooled_writer_retired.load(Ordering::Acquire) {
            return Err(SqliteError::InvalidData(
                "pooled writer connection retired after a terminal transaction fault".to_string(),
            ));
        }
        Ok(())
    }
}
