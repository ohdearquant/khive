//! File-backed standalone and queued SQLite writer operations.

use std::sync::Arc;

use async_trait::async_trait;
use khive_storage::error::StorageError;
use khive_storage::types::{PageRequest, SqlRow, SqlStatement, SqlValue};
use khive_storage::{StorageCapability, TopLevelMaintenance};

use crate::pool::ConnectionPool;

#[cfg(doc)]
use super::admit_standalone_operation;
use super::{
    admit_reader_capability_sql, bind_params, cached_read_transaction_control,
    execute_prepared_batch, execute_query_interruptibly, execute_query_page_interruptibly,
    execute_query_row_interruptibly, execute_standalone_read, execute_standalone_writer_query,
    map_write_err, open_explicit_read_transaction_handle, prepare_batch_statements,
    prepare_cached_sql_statement, reject_transaction_control_statements, run_pool_reader_query,
    run_standalone_batch, run_standalone_script, run_standalone_statement,
    run_standalone_top_level, transaction_control_head, writer_query_requires_admission,
    AtomicEventRows, CachedReadTransactionControl, StandaloneHandle, StandaloneWriteError,
};

// =============================================================================
// File-backed: SqliteWriter (standalone connection)
// =============================================================================

pub(super) struct SqliteWriter {
    // Manual atomic owners observe the final unit, excluding inner/cleanup errors.
    pub(super) observe_direct_errors: bool,
    /// `None` at construction when a `WriterTaskHandle` was obtained (ADR-136
    /// D1 gate 1: queue-first `writer()` skips the standalone open in that
    /// case). Ordinary `SqlReader` supertrait calls then use pooled readers;
    /// this slot is populated only while such a queue-backed handle owns an
    /// explicit deferred read transaction. An eagerly opened read-write
    /// connection (the no-writer-task branch) retains its one-permit writer
    /// budget for the handle's lifetime and must serve its reads on that same
    /// connection to preserve manual-transaction visibility.
    pub(super) handle: Option<StandaloneHandle>,
    /// ADR-067 Component A: when the write queue is enabled, `execute_batch`
    /// routes the whole caller-supplied statement list through the
    /// single-writer task instead of opening its own `BEGIN IMMEDIATE` on
    /// the standalone connection. `None` when the flag is off or no writer
    /// task is available
    /// (best-effort — degrades to the standalone-connection path below).
    pub(super) writer_task: Option<crate::writer_task::WriterTaskHandle>,
    /// The origin (ADR-091 backend-scoped attribution) of the pool this
    /// standalone connection was opened against.
    pub(super) origin: khive_storage::tx_registry::TxOrigin,
    /// This connection's pool's writer-timeout sink identity (`db_label`),
    /// captured at construction so the standalone-path busy/locked mapping
    /// below doesn't need a `&ConnectionPool` reference to report against.
    pub(super) db: String,
    /// Reader-pool owner and explicit-transaction exception source.
    pub(super) pool: Arc<ConnectionPool>,
    pub(super) event_rows: Option<Arc<AtomicEventRows>>,
}

pub(super) fn execute_top_level_maintenance(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
    maintenance: TopLevelMaintenance,
) -> rusqlite::Result<()> {
    match maintenance {
        TopLevelMaintenance::WalCheckpointTruncate => {
            let result = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            });
            crate::checkpoint::record_checkpoint_run_result(pool, result.as_ref().ok().copied());
            result.map(|_| ())
        }
        TopLevelMaintenance::Vacuum => conn.execute_batch(maintenance.as_sql()),
    }
}

impl SqliteWriter {
    /// A standalone handle holds its connection across calls, so opening it
    /// cannot serve as admission for every later write on that connection.
    /// Each write admits itself on its blocking thread
    /// ([`admit_standalone_operation`]); this only refuses a consumed handle
    /// before it is taken.
    fn require_standalone_handle(
        &self,
        operation: &'static str,
    ) -> khive_storage::types::StorageResult<()> {
        if self.handle.is_none() {
            return Err(StorageError::Pool {
                operation: operation.into(),
                message: "connection already consumed".into(),
            });
        }
        Ok(())
    }

    async fn use_queue_read_transaction_handle(
        &mut self,
        transaction_control: Option<CachedReadTransactionControl>,
        operation: &'static str,
    ) -> khive_storage::types::StorageResult<bool> {
        if self.handle.is_some() {
            return Ok(true);
        }
        match transaction_control {
            None => Ok(false),
            Some(CachedReadTransactionControl::BeginDeferred) => {
                self.handle =
                    Some(open_explicit_read_transaction_handle(Arc::clone(&self.pool)).await?);
                Ok(true)
            }
            Some(CachedReadTransactionControl::Finish(keyword))
            | Some(CachedReadTransactionControl::Unsupported(keyword)) => {
                Err(StorageError::InvalidInput {
                    capability: StorageCapability::Sql,
                    operation: operation.into(),
                    message: format!(
                        "cached read-only handle has no admitted transaction for transaction \
                         control ({keyword})"
                    ),
                })
            }
        }
    }

    fn close_inactive_queue_read_transaction_handle(&mut self) {
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_cached_reader() && !handle.has_read_transaction())
        {
            drop(self.handle.take());
        }
    }
}

#[async_trait]
impl khive_storage::SqlReader for SqliteWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        if self.writer_task.is_some() {
            let transaction_control = cached_read_transaction_control(&statement.sql);
            if !self
                .use_queue_read_transaction_handle(transaction_control, "writer.query_row")
                .await?
            {
                admit_reader_capability_sql(&statement, transaction_control, "writer.query_row")?;
                return run_pool_reader_query(
                    Arc::clone(&self.pool),
                    "writer.query_row",
                    move |scope, conn| {
                        execute_query_row_interruptibly(
                            scope,
                            conn,
                            &statement,
                            "writer.query_row",
                            false,
                            true,
                        )
                    },
                )
                .await;
            }
            let result = execute_standalone_read(
                &mut self.handle,
                Arc::clone(&self.pool),
                "writer.query_row",
                transaction_control,
                move |scope, conn, rollback, interruptible| {
                    execute_query_row_interruptibly(
                        scope,
                        conn,
                        &statement,
                        "writer.query_row",
                        rollback,
                        interruptible,
                    )
                },
            )
            .await;
            self.close_inactive_queue_read_transaction_handle();
            return result;
        }
        let requires_admission = writer_query_requires_admission(&statement);
        execute_standalone_writer_query(
            &mut self.handle,
            Arc::clone(&self.pool),
            "writer.query_row",
            requires_admission,
            move |scope, conn, rollback, interruptible| {
                execute_query_row_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "writer.query_row",
                    rollback,
                    interruptible,
                )
            },
        )
        .await
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        if self.writer_task.is_some() {
            let transaction_control = cached_read_transaction_control(&statement.sql);
            if !self
                .use_queue_read_transaction_handle(transaction_control, "writer.query_all")
                .await?
            {
                admit_reader_capability_sql(&statement, transaction_control, "writer.query_all")?;
                return run_pool_reader_query(
                    Arc::clone(&self.pool),
                    "writer.query_all",
                    move |scope, conn| {
                        execute_query_interruptibly(
                            scope,
                            conn,
                            &statement,
                            "writer.query_all",
                            false,
                            true,
                        )
                    },
                )
                .await;
            }
            let result = execute_standalone_read(
                &mut self.handle,
                Arc::clone(&self.pool),
                "writer.query_all",
                transaction_control,
                move |scope, conn, rollback, interruptible| {
                    execute_query_interruptibly(
                        scope,
                        conn,
                        &statement,
                        "writer.query_all",
                        rollback,
                        interruptible,
                    )
                },
            )
            .await;
            self.close_inactive_queue_read_transaction_handle();
            return result;
        }
        let requires_admission = writer_query_requires_admission(&statement);
        execute_standalone_writer_query(
            &mut self.handle,
            Arc::clone(&self.pool),
            "writer.query_all",
            requires_admission,
            move |scope, conn, rollback, interruptible| {
                execute_query_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "writer.query_all",
                    rollback,
                    interruptible,
                )
            },
        )
        .await
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        if self.writer_task.is_some() {
            let transaction_control = cached_read_transaction_control(&statement.sql);
            if !self
                .use_queue_read_transaction_handle(transaction_control, "writer.query_page")
                .await?
            {
                admit_reader_capability_sql(&statement, transaction_control, "writer.query_page")?;
                return run_pool_reader_query(
                    Arc::clone(&self.pool),
                    "writer.query_page",
                    move |scope, conn| {
                        execute_query_page_interruptibly(
                            scope,
                            conn,
                            &statement,
                            &page,
                            "writer.query_page",
                            false,
                            true,
                        )
                    },
                )
                .await;
            }
            let result = execute_standalone_read(
                &mut self.handle,
                Arc::clone(&self.pool),
                "writer.query_page",
                transaction_control,
                move |scope, conn, rollback, interruptible| {
                    execute_query_page_interruptibly(
                        scope,
                        conn,
                        &statement,
                        &page,
                        "writer.query_page",
                        rollback,
                        interruptible,
                    )
                },
            )
            .await;
            self.close_inactive_queue_read_transaction_handle();
            return result;
        }
        let requires_admission = writer_query_requires_admission(&statement);
        execute_standalone_writer_query(
            &mut self.handle,
            Arc::clone(&self.pool),
            "writer.query_page",
            requires_admission,
            move |scope, conn, rollback, interruptible| {
                execute_query_page_interruptibly(
                    scope,
                    conn,
                    &statement,
                    &page,
                    "writer.query_page",
                    rollback,
                    interruptible,
                )
            },
        )
        .await
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
impl khive_storage::SqlWriter for SqliteWriter {
    async fn execute(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<u64> {
        // A single statement is self-contained, just like execute_batch.
        // Manual atomic units use their own synchronous InlineWriter; this
        // cached handle never borrows a transaction-wide volume lease.
        // Route through the writer task when available. `self.handle` is
        // left untouched so a subsequent `execute`/`execute_script` call on
        // this same handle still works over the standalone connection.
        if let Some(writer_task) = self.writer_task.clone() {
            let event_rows = self.event_rows.clone();
            let labels =
                crate::writer_task::collect_statement_labels(std::slice::from_ref(&statement));
            return writer_task
                .send_bounded_labelled(
                    move |conn| {
                        let mut stmt = prepare_cached_sql_statement(conn, &statement.sql)
                            .map_err(|e| map_write_err(e, "execute"))?;
                        bind_params(&mut stmt, &statement.params)
                            .map_err(|e| map_write_err(e, "execute"))?;
                        let affected = stmt
                            .raw_execute()
                            .map_err(|e| map_write_err(e, "execute"))?;
                        if let Some(event_rows) = event_rows.as_deref() {
                            event_rows.observe(&statement, affected as u64);
                        }
                        Ok(affected as u64)
                    },
                    labels,
                )
                .await;
        }

        self.require_standalone_handle("execute")?;
        // A cached standalone statement owns the lease for this call only.
        // Cross-statement transactions belong to atomic_unit's owner.
        if let Some(keyword) = transaction_control_head(&statement.sql) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: "execute".into(),
                message: format!(
                    "statement is transaction control ({keyword}); a standalone \
                     statement holds the volume lease only for the call — use \
                     atomic_unit to run statements as one transaction"
                ),
            });
        }
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute".into(),
            message: "connection already consumed".into(),
        })?;
        let stage = match transaction_control_head(&statement.sql) {
            Some("BEGIN") => khive_storage::error::SqliteWriteStage::Begin,
            Some("COMMIT" | "END") => khive_storage::error::SqliteWriteStage::Commit,
            _ => khive_storage::error::SqliteWriteStage::Statement,
        };
        let event_rows = self.event_rows.clone();
        let pool = Arc::clone(&self.pool);
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_statement(handle, pool, statement, event_rows)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute", e))?;
        self.handle = Some(handle);
        let affected = result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(error) => self.map_direct_error(error, "execute", stage),
        })?;
        Ok(affected as u64)
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::types::StorageResult<u64> {
        // ADR-067 Component A: this call is self-contained (the full statement
        // list is supplied up front and the whole thing commits or rolls back
        // as one unit) — unlike `writer()`'s live incrementally-driven handle,
        // it maps cleanly onto a single `WriteRequest`. Route it through the
        // writer task when available; `self.handle` is left untouched so a
        // subsequent `execute`/`execute_script` call on this same handle still
        // works over the standalone connection (that dispatch is unmigrated —
        // see `SqlBridge::writer()`).
        //
        // Both paths reject transaction-control statements BEFORE executing
        // anything: the queue-backed branch runs inside the writer task's own
        // `BEGIN IMMEDIATE` (a caller `COMMIT` there would close the task's
        // transaction and terminate the writer task), and the standalone
        // branch below wraps the list in its own `BEGIN IMMEDIATE` (a caller
        // `COMMIT` would commit early and break all-or-nothing).
        reject_transaction_control_statements(&statements, "execute_batch")?;
        if let Some(writer_task) = self.writer_task.clone() {
            let event_rows = self.event_rows.clone();
            let labels = crate::writer_task::collect_statement_labels(&statements);
            return writer_task
                .send_bounded_labelled(
                    move |conn| {
                        let prepared = prepare_batch_statements(conn, &statements)
                            .map_err(|e| map_write_err(e, "execute_batch"))?;
                        execute_prepared_batch(conn, prepared, &statements, event_rows.as_deref())
                            .map_err(|e| map_write_err(e, "execute_batch"))
                    },
                    labels,
                )
                .await;
        }

        self.require_standalone_handle("execute_batch")?;
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_batch".into(),
            message: "connection already consumed".into(),
        })?;
        let origin = self.origin.clone();
        let event_rows = self.event_rows.clone();
        let pool = Arc::clone(&self.pool);
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_batch(handle, pool, statements, origin, event_rows)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_batch", e))?;
        self.handle = handle;
        result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(failure) => self.map_direct_batch_failure(failure),
        })
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        // ADR-067 Component A (Fork C slice 2): the script text is
        // self-contained (supplied up front, runs as one unit), just like
        // `execute_batch` — route it through the writer task when
        // available. `self.handle` is left untouched so a subsequent
        // `execute`/`execute_script` call on this same handle still works
        // over the standalone connection. Callers must supply a DML-only
        // script (no bare `BEGIN`/`COMMIT`/`ROLLBACK`) on the flag-on path,
        // since it runs inside the writer task's own transaction — same
        // Boundary: transaction-control rejection is an `execute_batch`
        // contract; this raw script path is internal/migration-only. The
        // queue-backed branch still requires a DML-only script because it
        // runs inside the writer task's transaction.
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send_bounded_at(
                    move |conn| {
                        conn.execute_batch(&script)
                            .map_err(|e| map_write_err(e, "execute_script"))
                    },
                    "execute_script",
                )
                .await;
        }

        self.require_standalone_handle("execute_script")?;
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script".into(),
            message: "connection already consumed".into(),
        })?;
        let pool = Arc::clone(&self.pool);
        let (handle, result) =
            tokio::task::spawn_blocking(move || run_standalone_script(handle, pool, script))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script", e))?;
        self.handle = handle;
        result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(error) => self.map_direct_error(
                error,
                "execute_script",
                khive_storage::error::SqliteWriteStage::Statement,
            ),
        })
    }

    async fn execute_script_top_level(
        &mut self,
        maintenance: TopLevelMaintenance,
    ) -> khive_storage::types::StorageResult<()> {
        // Only the closed maintenance enum can supply unbound SQL here.
        // This is not the separate raw migration-script interface.
        // ADR-067 Component A: unlike
        // `execute_script`, this must NOT run inside the writer task's
        // per-request `BEGIN IMMEDIATE` — statements such as VACUUM are
        // rejected by SQLite inside any open transaction. Route through
        // `WriterTaskHandle::send_top_level`, which still serializes this
        // call through the single writer owner but skips the transaction
        // wrap entirely.
        if let Some(writer_task) = self.writer_task.clone() {
            let pool = Arc::clone(&self.pool);
            let execute = move |conn: &rusqlite::Connection| {
                execute_top_level_maintenance(&pool, conn, maintenance)
                    .map_err(|e| map_write_err(e, "execute_script_top_level"))
            };
            return if maintenance == TopLevelMaintenance::WalCheckpointTruncate {
                writer_task.send_checkpoint_bounded(execute).await
            } else {
                writer_task.send_vacuum_bounded(execute).await
            };
        }

        // Flag off / no writer task: keep the volume lease through the
        // whole top-level operation. A checkpoint is a recovery bypass;
        // VACUUM uses a copy-sized DB/WAL metadata estimate.
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script_top_level".into(),
            message: "connection already consumed".into(),
        })?;
        let pool = Arc::clone(&self.pool);
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_top_level(handle, pool, maintenance)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script_top_level", e))?;
        self.handle = Some(handle);
        result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(error) => self.map_direct_error(
                error,
                "execute_script_top_level",
                khive_storage::error::SqliteWriteStage::Statement,
            ),
        })
    }
}
