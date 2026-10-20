use super::{
    acquire_handle_slot, async_trait, bind_params, execute_prepared_batch, execute_query,
    execute_query_page, execute_query_row, map_rusqlite_err, map_write_boundary_err, map_write_err,
    open_standalone_writer_on_blocking, prepare_batch_statements, prepare_cached_sql_statement,
    reject_transaction_control_statements, run_manual_atomic_unit, Any, Arc, AtomicEventRows,
    AtomicUnitOp, ConnectionPool, PageRequest, PoolBackedReader, PoolBackedWriter,
    SlotTimeoutClass, SqlRow, SqlStatement, SqlValue, SqliteError, SqliteReader, SqliteWriter,
    StorageCapability, StorageError,
};

// =============================================================================
// atomic_unit (ADR-067 Component A, Fork C slice 2)
// =============================================================================

/// A purely-synchronous `SqlReader`/`SqlWriter` over a borrowed connection,
/// used to drive an [`AtomicUnitOp`] on the queued or in-memory path, where the
/// closure body runs inside a `spawn_blocking` (synchronous
/// `FnOnce(&rusqlite::Connection) -> ...`) rather than a real async context.
///
/// Every method here does plain, non-suspending rusqlite work — there is no
/// real `.await` point anywhere in this impl — so [`block_on_sync`] driving
/// the resulting future to completion with a single poll is sound, not a
/// hack: the future can never actually be `Pending`.
///
/// `SqlReader`/`SqlWriter` both carry a `'static` supertrait bound (they are
/// used as `Box<dyn ...>` elsewhere in this module), so this type cannot
/// hold a real `&'c Connection` borrow — it would tie `InlineWriter` to a
/// non-`'static` lifetime and, independently, `&Connection` is not `Send`
/// (`Connection` is `!Sync`), which the `#[async_trait]`-generated futures
/// require. A raw pointer sidesteps both: `*const Connection` is `Send` and
/// `'static` on its face, and the safety burden (the pointee outliving
/// every dereference) is upheld by each atomic-unit owner: it builds an
/// `InlineWriter` from `conn: &Connection`, drives `op` to completion via
/// `block_on_sync` synchronously, and drops the `InlineWriter` before that
/// borrow ends, all within one stack frame.
pub(super) struct InlineWriter {
    pub(super) event_rows: Option<Arc<AtomicEventRows>>,
    pub(super) conn: *const rusqlite::Connection,
    pub(super) standalone_db: Option<String>,
}

// SAFETY: `InlineWriter` is never actually shared across a real thread
// boundary — it is constructed, driven to completion synchronously via
// `block_on_sync`, and dropped within a single call frame inside the
// `spawn_blocking` closure (see `atomic_unit`). The `Send`
// bound `async_trait` imposes on the futures below is a static
// over-approximation for this restricted, single-threaded usage pattern.
unsafe impl Send for InlineWriter {}

impl InlineWriter {
    /// SAFETY: valid for the lifetime of the enclosing synchronous scope in
    /// `atomic_unit` (see the struct doc comment above) — the pointee is
    /// never dereferenced after that scope ends.
    fn conn(&self) -> &rusqlite::Connection {
        unsafe { &*self.conn }
    }

    fn operation(&self, standalone: &'static str, inline: &'static str) -> &'static str {
        if self.standalone_db.is_some() {
            standalone
        } else {
            inline
        }
    }

    fn map_write_error(&self, error: rusqlite::Error, operation: &'static str) -> StorageError {
        if let Some(db) = &self.standalone_db {
            crate::timeout_sink::maybe_emit_busy(
                db,
                crate::timeout_sink::Site::StandaloneSqlBridge,
                &error,
            );
        }
        map_write_err(error, operation)
    }
}

#[async_trait]
impl khive_storage::SqlReader for InlineWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        execute_query_row(self.conn(), &statement).map_err(|error| {
            map_rusqlite_err(
                error,
                self.operation("writer.query_row", "inline.query_row"),
            )
        })
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        execute_query(self.conn(), &statement).map_err(|error| {
            map_rusqlite_err(
                error,
                self.operation("writer.query_all", "inline.query_all"),
            )
        })
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        execute_query_page(self.conn(), &statement, &page).map_err(|error| {
            map_rusqlite_err(
                error,
                self.operation("writer.query_page", "inline.query_page"),
            )
        })
    }

    async fn query_scalar(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlValue>> {
        let row = khive_storage::SqlReader::query_row(self, statement).await?;
        Ok(row.and_then(|r| r.columns.into_iter().next().map(|c| c.value)))
    }

    async fn explain(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let explain_stmt = SqlStatement {
            sql: format!("EXPLAIN QUERY PLAN {}", statement.sql),
            params: statement.params,
            label: statement.label,
        };
        khive_storage::SqlReader::query_all(self, explain_stmt).await
    }
}

#[async_trait]
impl khive_storage::SqlWriter for InlineWriter {
    async fn execute(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<u64> {
        // Boundary: `execute_batch` owns transaction-control rejection;
        // `atomic_unit` uses this one-statement primitive for its own boundary.
        let operation = self.operation("execute", "inline.execute");
        let mut stmt = prepare_cached_sql_statement(self.conn(), &statement.sql)
            .map_err(|error| self.map_write_error(error, operation))?;
        bind_params(&mut stmt, &statement.params)
            .map_err(|error| self.map_write_error(error, operation))?;
        let affected = stmt
            .raw_execute()
            .map_err(|error| self.map_write_error(error, operation))?;
        if let Some(event_rows) = self.event_rows.as_deref() {
            event_rows.observe(&statement, affected as u64);
        }
        Ok(affected as u64)
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::types::StorageResult<u64> {
        // Runs inside the writer task's per-request `BEGIN IMMEDIATE`
        // (atomic_unit flag-on path), so a caller `COMMIT` would close the
        // task's transaction — reject transaction-control statements up
        // front, same contract as every other `execute_batch`.
        let operation = self.operation("execute_batch", "inline.execute_batch");
        reject_transaction_control_statements(&statements, operation)?;
        let prepared = prepare_batch_statements(self.conn(), &statements)
            .map_err(|error| self.map_write_error(error, operation))?;
        execute_prepared_batch(
            self.conn(),
            prepared,
            &statements,
            self.event_rows.as_deref(),
        )
        .map_err(|error| self.map_write_error(error, operation))
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        // Boundary: this raw script path is internal maintenance only and is
        // outside the `execute_batch` transaction-control contract.
        self.conn().execute_batch(&script).map_err(|error| {
            self.map_write_error(
                error,
                self.operation("execute_script", "inline.execute_script"),
            )
        })
    }
}

/// Poll `fut` exactly once with a no-op waker and return its output.
///
/// Only sound for futures that never actually suspend — every caller in
/// this module drives an [`InlineWriter`], whose methods are pure
/// synchronous rusqlite calls with no real `.await` point.
///
/// ADR-067 Component A: this used to
/// `unreachable!()`-panic on `Poll::Pending`, and a panicking closure
/// running inside the writer task's `spawn_blocking` (see
/// `SqlBridge::atomic_unit`'s flag-on branch) would surface as a
/// `JoinError` in `run_writer_task`, which is treated as fatal — the writer
/// task exits and every subsequent `WriterTaskHandle::send` on this pool
/// fails for the rest of the process. A future `atomic_unit` caller whose
/// closure ever gains a real suspend point (this file's own contract
/// already forbids it, but the invariant is enforced by convention, not the
/// type system) would take down the writer task for the whole daemon.
/// Returning `Err` instead lets `Pending` flow through the SAME error path
/// as any other `atomic_unit` op failure: `WriteRequest::execute_and_reply`
/// treats it as an ordinary `Err`, issues `ROLLBACK` on the writer task's
/// held transaction, replies the error to the caller, and the writer task's
/// `spawn_blocking` closure returns normally (not via panic) — so the task
/// keeps draining subsequent requests instead of dying with the whole pool.
pub(super) fn block_on_sync<F: std::future::Future>(fut: F) -> Result<F::Output, StorageError> {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn no_op(_: *const ()) {}
    fn clone_waker(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_waker, no_op, no_op, no_op);

    // SAFETY: every `RawWakerVTable` function is a no-op that never
    // dereferences the data pointer, so a null data pointer is sound.
    let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw_waker) };
    let mut cx = Context::from_waker(&waker);

    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Ok(v),
        Poll::Pending => {
            tracing::error!(target: "khive_db::sql_bridge",
                "block_on_sync: atomic_unit future suspended on its first poll — \
                 the closure passed to SqlAccess::atomic_unit must be non-blocking \
                 (synchronous InlineWriter calls only, no real .await point)"
            );
            Err(StorageError::Internal(
                "atomic_unit future suspended — closure must be non-blocking".to_string(),
            ))
        }
    }
}

// =============================================================================
// SqlBridge: the SqlAccess implementor
// =============================================================================

/// Bridges `ConnectionPool` to `khive_storage::SqlAccess`.
///
/// Dispatches based on whether the pool is file-backed or in-memory:
/// - File-backed: pooled ordinary reader operations and a lazy standalone
///   connection only for explicitly admitted multi-call read transactions,
///   plus standalone writer connections capped at one live handle; atomic
///   units drive a single registered raw transaction span.
/// - In-memory: pool-backed connections per query (single shared connection).
pub struct SqlBridge {
    pool: Arc<ConnectionPool>,
    pub(super) is_file_backed: bool,
}

impl SqlBridge {
    /// Create a new bridge wrapping the given pool.
    pub fn new(pool: Arc<ConnectionPool>, _is_file_backed: bool) -> Self {
        // The legacy hint is not an authority for choosing an unguarded
        // in-memory writer. A caller cannot route a file-backed pool through
        // PoolBackedWriter by supplying `false`.
        let is_file_backed = pool.canonical_path().is_some();
        Self {
            pool,
            is_file_backed,
        }
    }
}

#[async_trait]
impl khive_storage::SqlAccess for SqlBridge {
    fn database_path(&self) -> Option<std::path::PathBuf> {
        self.pool.canonical_path().map(std::path::Path::to_path_buf)
    }

    async fn reader(
        &self,
    ) -> khive_storage::types::StorageResult<Box<dyn khive_storage::SqlReader>> {
        if self.is_file_backed {
            Ok(Box::new(SqliteReader {
                handle: None,
                pool: Arc::clone(&self.pool),
                poisoned: false,
            }))
        } else {
            Ok(Box::new(PoolBackedReader {
                pool: Arc::clone(&self.pool),
                transaction: None,
            }))
        }
    }

    async fn writer(
        &self,
    ) -> khive_storage::types::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        if self.is_file_backed {
            if self.pool.config().read_only {
                return Err(StorageError::Pool {
                    operation: "writer".into(),
                    message: "backend is read-only".into(),
                });
            }
            let db = crate::timeout_sink::db_label(&self.pool);
            // ADR-136 D1 gate 1: queue-first. The handle lookup runs BEFORE
            // any standalone connection is opened, and a lookup failure is
            // propagated (never silently degraded) when strict routing is
            // on. Only the flag-off/degraded case still opens a standalone
            // connection.
            let writer_task = match self.pool.writer_task_handle() {
                Ok(handle) => handle,
                Err(e) => {
                    if self.pool.config().write_routing_strict {
                        return Err(e);
                    }
                    tracing::warn!(target: "khive_db::sql_bridge",
                        error = %e,
                        "KHIVE_WRITE_ROUTING is not strict; writer() degrades to the \
                         standalone-connection path"
                    );
                    None
                }
            };
            if writer_task.is_none() && self.pool.config().write_routing_strict {
                return Err(StorageError::Pool {
                    operation: "writer".into(),
                    message: "KHIVE_WRITE_ROUTING=strict but no writer-task handle is \
                              available; refusing to fall back to a direct connection"
                        .into(),
                });
            }
            if writer_task.is_none() && self.pool.write_queue_active() {
                // The queue is enabled but this call didn't get a handle
                // (spawn/runtime degrade) — a direct-route violation in the
                // making once this writer's execute*/query* methods run.
                // In-memory pools are excluded: they never spawn a writer
                // task by documented design (explicit `Some(true)` degrades),
                // so a violation row there would be noise, not signal.
                crate::timeout_sink::emit_direct_route_violation(
                    &db,
                    crate::timeout_sink::Site::DirectRouteSqlBridgeWriter,
                );
            }
            // A standalone read-write connection is opened only when there is
            // no queue handle to route writes through — `SqliteWriter`'s
            // `SqlReader` methods (`query_row`/`query_all`/`query_page`)
            // use pooled readers in the queue-backed case and lazily open the
            // closed standalone exception only for an explicit deferred read
            // transaction. Production callers do read through a `writer()`
            // handle, so both routes are live.
            // The standalone open acquires the pool-wide one-permit writer
            // budget first, and the permit travels in the handle for the
            // handle's whole lifetime — a queue-backed handle holds no
            // writer permit (its writes route through the writer task), so
            // this budget caps exactly the standalone read-write
            // connections.
            if writer_task.is_none() {
                let handle_slot = acquire_handle_slot(
                    self.pool.sql_bridge_writer_slots(),
                    self.pool.config().checkout_timeout,
                    "sql_bridge.writer_handle",
                    SlotTimeoutClass::Admission,
                )
                .await?;
                return open_standalone_writer_on_blocking(Arc::clone(&self.pool), handle_slot)
                    .await;
            }
            Ok(Box::new(SqliteWriter {
                observe_direct_errors: true,
                event_rows: None,
                handle: None,
                writer_task,
                origin: self.pool.origin(),
                db,
                pool: Arc::clone(&self.pool),
            }))
        } else {
            Ok(Box::new(PoolBackedWriter {
                pool: Arc::clone(&self.pool),
            }))
        }
    }

    /// Implements the trait's atomic-unit suspend-free invariant
    /// (`SqlAccess::atomic_unit`'s doc comment): on the queued and in-memory branches,
    /// `op` is driven through `block_on_sync` on an `InlineWriter` — a
    /// single-poll driver that returns `Err` the instant `op`'s future is
    /// `Pending` instead of ever actually suspending. `op` must therefore
    /// issue only synchronous DML; see `InlineWriter`'s and
    /// `block_on_sync`'s doc comments for the full mechanics and why this
    /// restriction is load-bearing (a suspended poll inside the writer
    /// task's `spawn_blocking` would otherwise block that task on external
    /// async work while holding the single write connection).
    async fn atomic_unit(
        &self,
        op: AtomicUnitOp,
    ) -> khive_storage::types::StorageResult<Box<dyn Any + Send>> {
        let event_rows = Arc::new(AtomicEventRows::default());
        let result = async {
            if self.is_file_backed {
                if self.pool.config().read_only {
                    return Err(StorageError::Pool {
                        operation: "atomic_unit".into(),
                        message: "backend is read-only".into(),
                    });
                }
                // Best-effort, same guard `writer()` uses: `Ok(None)` on flag-off;
                // `Err(WriterTaskNoRuntime)` propagates loud rather than silently
                // falling back to a competing connection from a sync caller. ADR-136
                // D1 gate 3: `Ok(None)` under strict routing is ALSO a fail-closed
                // error (queue was requested but unavailable), not just a degrade.
                let handle = self.pool.writer_task_handle()?;
                if handle.is_none() && self.pool.config().write_routing_strict {
                    return Err(StorageError::Pool {
                        operation: "atomic_unit".into(),
                        message: "KHIVE_WRITE_ROUTING=strict but no writer-task handle is \
                              available; refusing to fall back to a direct connection"
                            .into(),
                    });
                }
                if handle.is_none() && self.pool.write_queue_active() {
                    crate::timeout_sink::emit_direct_route_violation(
                        &crate::timeout_sink::db_label(&self.pool),
                        crate::timeout_sink::Site::DirectRouteAtomicUnit,
                    );
                }
                if let Some(writer_task) = handle {
                    // Flag-on: ONE queued WriteRequest. `run_writer_task` already
                    // has an open `BEGIN IMMEDIATE` on its dedicated connection
                    // before this closure runs and issues `COMMIT`/`ROLLBACK`
                    // after it returns — `op` must not (and, via `InlineWriter`,
                    // does not) issue its own transaction control.
                    let pending_event_rows = Arc::clone(&event_rows);
                    return writer_task
                        .send_bounded_at(
                            move |conn| {
                                let mut inline = InlineWriter {
                                    event_rows: Some(Arc::clone(&pending_event_rows)),
                                    conn: conn as *const rusqlite::Connection,
                                    standalone_db: None,
                                };
                                // Flatten: `block_on_sync` now returns `Result<F::Output,
                                // StorageError>` (outer = "did the future actually
                                // resolve on first poll", inner = the op's own
                                // `StorageResult`) instead of panicking on `Pending`
                                // (ADR-067 Component A). Either
                                // error flows through this closure's ordinary `Err`
                                // return, which `WriteRequest::execute_and_reply`
                                // already turns into a normal ROLLBACK + error reply —
                                // no panic, so the writer task survives.
                                match block_on_sync(op(&mut inline)) {
                                    Ok(inner) => inner,
                                    Err(e) => Err(e),
                                }
                            },
                            "atomic_unit",
                        )
                        .await;
                }
                // Contract: this acquire waits on the pool-wide one-permit
                // writer-handle budget — the same permit a live `writer()` handle
                // holds for its lifetime — so it times out with
                // `StorageError::AdmissionTimeout` after `checkout_timeout` while a writer
                // handle is checked out (and a `writer()` call times out while
                // this unit runs). Callers must not hold a boxed writer handle
                // across an `atomic_unit()` call on the same pool; drop the
                // handle first. The `writer_task` branch above never touches this
                // budget.
                let handle_slot = acquire_handle_slot(
                    self.pool.sql_bridge_writer_slots(),
                    self.pool.config().checkout_timeout,
                    "sql_bridge.atomic_unit_handle",
                    SlotTimeoutClass::Admission,
                )
                .await?;
                run_manual_atomic_unit(
                    Arc::clone(&self.pool),
                    handle_slot,
                    op,
                    Arc::clone(&event_rows),
                )
                .await
                .inspect_err(|error| self.pool.record_direct_writer_error(error))
            } else {
                // Every statement shares one connection. Keep its guard through
                // commit/rollback so other units and ordinary writes cannot join it.
                let pool = Arc::clone(&self.pool);
                let pending_event_rows = Arc::clone(&event_rows);
                tokio::task::spawn_blocking(move || {
                    let guard = pool.try_writer().map_err(|error: SqliteError| {
                        StorageError::driver(StorageCapability::Sql, "atomic_unit", error)
                    })?;
                    let conn = guard.conn();
                    if !conn.is_autocommit() {
                        pool.retire_pooled_writer(conn);
                        return Err(StorageError::writer_task_terminated(
                            khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                        ));
                    }
                    if let Err(error) = conn.execute_batch("BEGIN IMMEDIATE") {
                        if !conn.is_autocommit() {
                            pool.retire_pooled_writer(conn);
                            return Err(SqliteError::settlement_with_cause(
                                &error,
                                khive_storage::error::SqliteWriteStage::Begin,
                            )
                            .into_storage_error(StorageCapability::Sql, "atomic_unit.begin"));
                        }
                        return Err(map_write_boundary_err(
                            error,
                            "atomic_unit.begin",
                            khive_storage::error::SqliteWriteStage::Begin,
                        ))
                        .inspect_err(|error| pool.record_direct_writer_error(error));
                    }
                    let _tx_handle = khive_storage::tx_registry::register_scoped(
                        Some("atomic_unit".to_string()),
                        pool.origin(),
                    );
                    let (result, terminal_state) = crate::writer_task::execute_wrapped_transaction(
                        conn,
                        "atomic_unit.commit",
                        |conn| {
                            let mut inline = InlineWriter {
                                event_rows: Some(Arc::clone(&pending_event_rows)),
                                conn: conn as *const rusqlite::Connection,
                                standalone_db: None,
                            };
                            block_on_sync(op(&mut inline)).and_then(|result| result)
                        },
                    );
                    if terminal_state.is_some() {
                        pool.retire_pooled_writer(conn);
                    }
                    result.inspect_err(|error| pool.record_direct_writer_error(error))
                })
                .await
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "atomic_unit", error)
                })?
            }
        }
        .await;
        khive_storage::usage::account_event_write(
            result.as_ref().map(|_| event_rows.committed_rows()),
        );
        result
    }
}
