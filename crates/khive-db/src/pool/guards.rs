//! Reader lease capabilities, cleanup, and the owned shared-reader transaction.

use std::cell::Cell;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use khive_storage::{error::StorageError, StorageCapability};
use rusqlite::Connection;

use crate::error::SqliteError;

use super::{
    close_connection_quietly, pool_exhausted_error, restore_shared_reader_state, ConnectionPool,
    PoolConfig,
};

#[derive(Debug)]
pub(super) struct PooledReader {
    pub(super) conn: Connection,
    pub(super) opened_at: Instant,
    pub(super) checkouts: u64,
}

impl PooledReader {
    pub(super) fn past_limit(&self, config: &PoolConfig, now: Instant) -> bool {
        now.saturating_duration_since(self.opened_at) > config.reader_max_age
            || self.checkouts > config.reader_max_ops
    }
}

pub(super) enum ReaderLease<'pool> {
    Pooled(Box<PooledReader>),
    Shared(parking_lot::MutexGuard<'pool, Connection>),
}

/// A value-extraction view of one row from a reader lease.
///
/// It deliberately exposes neither the prepared statement nor its connection.
///
/// ```compile_fail
/// fn statement(row: &khive_db::ReaderRow<'_, '_>) {
///     let _: &rusqlite::Statement<'_> = row.as_ref();
/// }
/// ```
pub struct ReaderRow<'row, 'statement> {
    row: &'row rusqlite::Row<'statement>,
}

impl ReaderRow<'_, '_> {
    /// Extract a value by zero-based column index or column name.
    pub fn get<I: rusqlite::RowIndex, T: rusqlite::types::FromSql>(
        &self,
        index: I,
    ) -> rusqlite::Result<T> {
        self.row.get(index)
    }

    /// Borrow a SQLite value without exposing statement metadata or execution.
    pub fn get_ref<I: rusqlite::RowIndex>(
        &self,
        index: I,
    ) -> rusqlite::Result<rusqlite::types::ValueRef<'_>> {
        self.row.get_ref(index)
    }
}

/// One public query owns this lease's connection-global progress handler.
struct ReaderQueryInProgress<'a>(&'a Cell<bool>);

impl Drop for ReaderQueryInProgress<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// One pool-wide reader admission permit acquired before any connection is
/// selected, with the instant its checkout wait began so a single
/// `checkout_timeout` bounds both the permit wait and the connection pick.
/// Hand it to [`ConnectionPool::reader_with_admission`], which moves the permit
/// into the resulting [`ReaderGuard`].
pub(crate) struct ReaderAdmission {
    pub(super) slot: tokio::sync::OwnedSemaphorePermit,
    pub(super) started: Instant,
}

/// A reader connection checked out from the pool.
/// Returns the connection to the pool on drop.
pub struct ReaderGuard<'pool> {
    pub(super) lease: Option<ReaderLease<'pool>>,
    /// One permit from the pool-wide reader budget, shared with the explicit
    /// raw-SQL transaction exception. Returned only after the connection has
    /// been reset/replaced and made reusable.
    pub(super) admission_slot: Option<tokio::sync::OwnedSemaphorePermit>,
    pub(super) pool: &'pool ConnectionPool,
    pub(super) reusable: Cell<bool>,
    pub(super) query_in_progress: Cell<bool>,
    pub(super) checked_out_at: Instant,
    /// Set by [`Self::mark_dirty`] whenever this checkout ran a `SqlReader`
    /// raw-SQL statement (`sql_bridge`'s `run_pool_reader_query`), never by a
    /// typed store read. Gates whether `Drop` pays the TEMP-object,
    /// attachment, and connection-setting pristine scan on return —
    /// `reader_connection_state_is_pristine` and
    /// `reader_connection_settings_match_baseline` never touch the hot path
    /// of an ordinary typed checkout.
    pub(super) dirty: Cell<bool>,
    /// Names the typed-store operation this checkout was resolved for, set
    /// once by [`ConnectionPool::resolve_reader_checkout`]. `None` means the
    /// checkout never passed that route (the pool-internal and raw-SQL
    /// callers), and the diagnostics maximum reports it as unattributed
    /// rather than guessing.
    pub(super) operation: Option<&'static str>,
}

impl<'pool> ReaderGuard<'pool> {
    /// Access the connection from within `khive-db`. Every internal caller
    /// is either a typed store (proven read-only by construction) or a
    /// raw-SQL route that already calls [`Self::mark_dirty`] itself, so this
    /// stays crate-private: it is the untracked half of the pristine-return
    /// contract, and a caller outside the crate has no way to pay into that
    /// contract by calling `mark_dirty` (also crate-private). A raw
    /// `&Connection` is never handed to a caller outside `khive-db` — use
    /// [`Self::query_row`] instead, which admits only read-shaped SQL.
    pub(crate) fn conn(&self) -> &Connection {
        match self
            .lease
            .as_ref()
            .expect("reader guard missing connection")
        {
            ReaderLease::Pooled(reader) => &reader.conn,
            ReaderLease::Shared(guard) => guard,
        }
    }

    /// Run one read-only statement against this reader lease and map the
    /// first resulting row, for callers outside `khive-db`.
    ///
    /// Unlike a raw `Connection`, this never hands out a capability that can
    /// change connection-local or database state: `sql` is checked against
    /// the same allow-listed read-shape classifier
    /// (`SELECT`/`WITH ... SELECT`/`VALUES`/`EXPLAIN`/a fixed read-only
    /// `PRAGMA` set) the pooled `SqlReader` surface admits raw SQL through,
    /// and anything else — `BEGIN`, DML, DDL, a setting `PRAGMA`, `ATTACH`
    /// — is refused before it ever reaches SQLite. An admitted statement
    /// still marks the checkout dirty unconditionally, so `Drop` always pays
    /// the pristine-state scan (or, in degraded shared-lease mode, the
    /// settings/rollback verification) on return.
    ///
    /// SQL stepping cooperatively observes the current request's cancellation
    /// and original absolute deadline. Synchronous mapper/native callback code
    /// cannot be forcibly preempted; a post-check refuses a successful result
    /// if the request stopped while that code ran.
    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T, SqliteError>
    where
        P: rusqlite::Params,
        F: FnOnce(&ReaderRow<'_, '_>) -> rusqlite::Result<T>,
    {
        crate::sql_bridge::reader_capability_admits(sql).map_err(SqliteError::InvalidData)?;
        if !self.reusable.get() {
            return Err(SqliteError::InvalidData(
                "reader lease is quarantined after failed read cleanup".into(),
            ));
        }
        // Params may call user-provided ToSql before stepping, and the mapper
        // also runs user code. Neither may replace this query's progress handler
        // by recursively querying the same lease.
        if self.query_in_progress.replace(true) {
            return Err(SqliteError::InvalidData(
                "reader lease is already executing a query".into(),
            ));
        }
        let _in_progress = ReaderQueryInProgress(&self.query_in_progress);
        self.mark_dirty();
        crate::read_cancellation::run_borrowed_reader(self, |conn, admission| {
            conn.query_row(sql, params, |row| {
                // Parameter conversion runs before any stepping, and a query
                // cheap enough not to trip the progress handler never polls at
                // all, so this is the only point that can keep the documented
                // guarantee: a cancelled request does not enter the mapper.
                // SQLITE_INTERRUPT is the code the scope already recognises, so
                // the refusal converts to the same timeout error as a
                // cancellation observed during stepping.
                if !admission.admits() {
                    return Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                        Some("request stopped before the mapper".into()),
                    ));
                }
                f(&ReaderRow { row })
            })
            .map_err(|error| {
                StorageError::driver(StorageCapability::Sql, "reader_guard.query_row", error)
            })
        })
        .map_err(|error| {
            self.pool.record_reader_query_error(&error);
            match error {
                StorageError::Driver {
                    capability,
                    operation,
                    source,
                } => match source.downcast::<rusqlite::Error>() {
                    Ok(error) => SqliteError::Rusqlite(*error),
                    Err(source) => SqliteError::RequestReadStopped(StorageError::Driver {
                        capability,
                        operation,
                        source,
                    }),
                },
                other => SqliteError::RequestReadStopped(other),
            }
        })
    }

    /// Fail closed when connection-global state could not be restored after
    /// a read. A pooled reader is closed and replaced on drop; a degraded
    /// shared-writer reader is quarantined for the lifetime of the pool.
    pub(crate) fn discard(&self) {
        self.reusable.set(false);
    }

    /// Mark this checkout as having run a raw-SQL statement, so `Drop` pays
    /// the pristine-state scan on return instead of skipping it.
    pub(crate) fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    /// Name the typed-store operation this checkout serves, so a long hold
    /// can be attributed in diagnostics instead of arriving as a bare
    /// maximum with no next step (#2793).
    pub(crate) fn label_operation(&mut self, operation: &'static str) {
        self.operation = Some(operation);
    }
}

impl<'pool> Drop for ReaderGuard<'pool> {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };

        match lease {
            ReaderLease::Pooled(conn) if self.reusable.get() => {
                self.pool.return_reader(conn, self.dirty.get())
            }
            ReaderLease::Pooled(conn) => {
                close_connection_quietly(conn.conn);
                self.pool.replace_discarded_reader_slot();
            }
            ReaderLease::Shared(guard) if !self.reusable.get() => {
                self.pool.retire_pooled_writer(&guard);
            }
            ReaderLease::Shared(guard) => {
                // The shared lease IS the pool's writer connection (degraded
                // `max_readers == 0` mode) — there is no separate reader
                // connection to close and replace. A dirty return that
                // cannot be verifiably restored poisons the whole pool via
                // the same terminal-fault path a writer transaction fault
                // uses, since reuse and replacement are equally unavailable
                // here.
                if self.dirty.get() && !restore_shared_reader_state(&guard, &self.pool.config) {
                    self.pool.retire_pooled_writer(&guard);
                }
            }
        }

        // A checkout remains active until reset/replacement returned the
        // connection to service (or the degraded writer guard was released),
        // not merely until the caller's query closure returned.
        drop(self.admission_slot.take());
        self.pool
            .reader_acquisition_counters
            .record_checkout_completed(self.checked_out_at.elapsed(), self.operation);
        self.pool.finish_reader_checkout(self.checked_out_at);
    }
}

/// Owned, `'static` counterpart to [`ReaderGuard`]'s degraded-mode
/// (`max_readers == 0`) lease. `ReaderGuard` borrows `&'pool ConnectionPool`
/// and therefore cannot be retained across the separate `.await` points
/// between a caller's `SqlReader` trait-method calls — every ordinary call
/// through it draws a fresh checkout and returns it before the next call
/// begins. That is fine for an ordinary read, but wrong for the explicit
/// multi-call deferred read transaction (ADR-005/ADR-091): a `BEGIN
/// DEFERRED` checked out and returned this way releases the pool's one
/// physical connection to any concurrent caller — including a writer —
/// before its own matching `COMMIT`/`ROLLBACK` runs, and an abandoned span
/// (an error or cancellation between the two) leaves that connection sitting
/// in the pool inside an open transaction with nothing left holding it. This
/// type owns an `Arc`-rooted mutex guard instead of borrowing one, so it can
/// be held by the caller across those `.await` points and give the whole
/// span real, exclusive ownership of the one connection.
pub(crate) struct SharedReaderTransactionGuard {
    conn: parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>,
    admission_slot: Option<tokio::sync::OwnedSemaphorePermit>,
    pool: Arc<ConnectionPool>,
    checked_out_at: Instant,
    /// Set once a statement executed against this connection could not
    /// prove its own cleanup ran (SQLite progress-handler removal failure).
    /// `Drop` then treats the connection exactly like a rollback failure:
    /// poison the pool rather than let possibly-corrupted connection state
    /// return to service.
    poison: Cell<bool>,
}

impl SharedReaderTransactionGuard {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Force poisoning on drop regardless of the connection's own
    /// autocommit/pristine state.
    pub(crate) fn poison(&self) {
        self.poison.set(true);
    }
}

impl Drop for SharedReaderTransactionGuard {
    fn drop(&mut self) {
        // An abandoned span must never let this connection return to
        // service while still inside an open transaction — the next reader
        // or writer to draw the pool's one physical connection would
        // silently inherit it. Roll back first; only a verified return to
        // autocommit, followed by the same pristine-state scan an ordinary
        // dirty degraded checkout pays, allows reuse.
        let mut restored = !self.poison.get();
        if restored && !self.conn.is_autocommit() {
            restored = self.conn.execute_batch("ROLLBACK").is_ok() && self.conn.is_autocommit();
        }
        if restored {
            restored = restore_shared_reader_state(&self.conn, &self.pool.config);
        }
        if !restored {
            self.pool.retire_pooled_writer(&self.conn);
        }
        drop(self.admission_slot.take());
        self.pool
            .reader_acquisition_counters
            .record_checkout_completed(
                self.checked_out_at.elapsed(),
                Some("explicit_sql_read_transaction"),
            );
        self.pool.finish_reader_checkout(self.checked_out_at);
    }
}

impl ConnectionPool {
    /// Check out the degraded-mode (`max_readers == 0`) pool's single
    /// physical connection as an owned [`SharedReaderTransactionGuard`]
    /// instead of a borrowed [`ReaderGuard`]. Mirrors the admission and
    /// mutex-acquisition loop in [`Self::reader_until`]'s degraded branch —
    /// the only difference is the guard type returned, so a caller can
    /// retain this one across several separate `.await` points.
    ///
    /// Exists only for the in-memory backend's explicit deferred-read-
    /// transaction bypass (`sql_bridge::PoolBackedReader`); ordinary reads
    /// use [`Self::reader_until`].
    pub(crate) fn checkout_shared_reader_transaction(
        self: &Arc<Self>,
        should_stop: impl Fn() -> bool,
    ) -> Result<Option<SharedReaderTransactionGuard>, SqliteError> {
        debug_assert_eq!(
            self.max_readers, 0,
            "the owned shared-reader-transaction guard exists only for the degraded, \
             single-connection backend"
        );
        self.ensure_pooled_writer_active()?;
        let started = Instant::now();
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
            thread::yield_now();
        };

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
            if let Some(conn) = self
                .writer
                .try_lock_arc_for(remaining.min(Duration::from_millis(2)))
            {
                self.ensure_pooled_writer_active()?;
                self.reader_acquisition_counters.record_pooled_checkout();
                return Ok(Some(SharedReaderTransactionGuard {
                    conn,
                    admission_slot: Some(admission_slot),
                    pool: Arc::clone(self),
                    checked_out_at: self.begin_reader_checkout(),
                    poison: Cell::new(false),
                }));
            }
        }
    }
}
