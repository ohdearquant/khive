use super::{
    admit_reader_capability_sql, async_trait, bind_params, cached_read_transaction_control,
    execute_prepared_batch, execute_query_interruptibly, execute_query_page_interruptibly,
    execute_query_row_interruptibly, map_write_boundary_err, map_write_err,
    prepare_batch_statements, prepare_cached_sql_statement, reject_transaction_control_statements,
    settle_pooled_call, transaction_control_head, writer_query_requires_admission, Arc,
    CachedReadTransactionControl, ConnectionPool, PageRequest, SharedReaderTransactionGuard,
    SqlRow, SqlStatement, SqlValue, SqliteError, StorageCapability, StorageError,
};

// =============================================================================
// Pool-backed reader/writer (in-memory databases)
// =============================================================================

pub(super) async fn run_pool_reader_query<T, F>(
    pool: Arc<ConnectionPool>,
    operation: &'static str,
    query: F,
) -> khive_storage::types::StorageResult<T>
where
    T: Send + 'static,
    F: FnOnce(
            &crate::read_cancellation::InterruptibleReadScope,
            &rusqlite::Connection,
        ) -> khive_storage::types::StorageResult<T>
        + Send
        + 'static,
{
    // Await the permit before the blocking task starts: a queued read holds no thread.
    let admission = pool
        .acquire_reader_admission(StorageCapability::Sql, operation)
        .await?;
    crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            // Checkout tri-state (cancelled -> Timeout, admission expiry ->
            // retryable AdmissionTimeout, other -> Driver) lives in ONE place:
            // `ConnectionPool::resolve_reader_checkout`.
            let mut guard = pool.resolve_reader_checkout(
                StorageCapability::Sql,
                operation,
                pool.reader_with_admission(admission, || scope.should_stop()),
            )?;
            // Every caller of `run_pool_reader_query` runs a `SqlStatement`
            // (raw SQL) rather than a typed store's fixed query, so the
            // checkout pays the pristine-state scan on return regardless of
            // which `SqlReader` wrapper (reader or writer capability) drew it.
            guard.mark_dirty();
            let result = scope.with_pooled_reader(&mut guard, |conn| query(scope, conn));
            if let Err(error) = &result {
                pool.record_reader_query_error(error);
            }
            result
        },
    )
    .await
}

async fn run_pool_writer_query<T, F>(
    pool: Arc<ConnectionPool>,
    operation: &'static str,
    requires_admission: bool,
    query: F,
) -> khive_storage::types::StorageResult<T>
where
    T: Send + 'static,
    F: FnOnce(
            &crate::read_cancellation::InterruptibleReadScope,
            &rusqlite::Connection,
            bool,
        ) -> khive_storage::types::StorageResult<T>
        + Send
        + 'static,
{
    crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            let guard = pool.try_writer().map_err(|error: SqliteError| {
                error.into_storage_error(StorageCapability::Sql, operation)
            })?;
            if requires_admission {
                let unit = guard
                    .admit_autocommit()
                    .map_err(|error| error.into_storage_error(StorageCapability::Sql, operation))?;
                let result = scope.with_pooled_write_unit(&pool, &unit, |conn| {
                    query(scope, conn, conn.is_autocommit())
                });
                return settle_pooled_call(&unit, operation, result)
                    .inspect_err(|error| pool.record_direct_writer_error(error));
            }
            scope.with_pooled_writer(&pool, &guard, |conn| {
                let interruptible = conn.is_autocommit();
                query(scope, conn, interruptible)
            })
        },
    )
    .await
}

pub(super) struct PoolBackedReader {
    pub(super) pool: Arc<ConnectionPool>,
    /// Present only while an admitted explicit deferred read transaction
    /// (ADR-005/ADR-091) is open on the in-memory backend's single shared
    /// connection. Retaining it here — instead of drawing a fresh checkout
    /// per call, the way an ordinary read does — is what gives the span
    /// real connection ownership: see [`SharedReaderTransactionGuard`] and
    /// [`run_pool_backed_reader_query`].
    pub(super) transaction: Option<SharedReaderTransactionGuard>,
}

/// Decide what happened to a [`SharedReaderTransactionGuard`] after one
/// statement ran against it and either retain it in `*transaction` (still
/// mid-span) or let it drop naturally (span finished cleanly).
///
/// `expect_open_after` is the caller's terminal-state expectation: `true`
/// for an ordinary read inside an already-open span (the transaction must
/// still be open afterward), `false` for the statement that opens or closes
/// the span. A mismatch poisons the guard — it is never handed back for
/// reuse — and is folded into the returned error so a broken span is never
/// reported as a successful read.
fn finish_pool_backed_reader_step<T>(
    transaction: &mut Option<SharedReaderTransactionGuard>,
    guard: SharedReaderTransactionGuard,
    expect_open_after: bool,
    operation: &'static str,
    result: khive_storage::types::StorageResult<T>,
) -> khive_storage::types::StorageResult<T> {
    let still_open = !guard.conn().is_autocommit();
    if still_open == expect_open_after {
        if still_open {
            *transaction = Some(guard);
        }
        // Otherwise the span finished cleanly (or never opened); let `guard`
        // drop here, returning the connection to the pool.
        return result;
    }
    guard.poison();
    let message = if expect_open_after {
        "a read inside the pool-backed reader's admitted transaction unexpectedly ended it; \
         the connection was discarded"
    } else {
        "transaction-ending control completed but the pool-backed reader's connection \
         remained outside autocommit; the connection was discarded"
    };
    match result {
        Err(error) => Err(error),
        Ok(_) => Err(StorageError::InvalidInput {
            capability: StorageCapability::Sql,
            operation: operation.into(),
            message: message.into(),
        }),
    }
}

/// Open the explicit deferred read-transaction span on the in-memory
/// backend's shared connection and run the admitted `BEGIN DEFERRED`
/// statement against it.
async fn open_pool_backed_reader_transaction<T, F>(
    transaction: &mut Option<SharedReaderTransactionGuard>,
    pool: Arc<ConnectionPool>,
    operation: &'static str,
    query: F,
) -> khive_storage::types::StorageResult<T>
where
    T: Send + 'static,
    F: FnOnce(
            &crate::read_cancellation::InterruptibleReadScope,
            &rusqlite::Connection,
            bool,
            bool,
        ) -> khive_storage::types::StorageResult<T>
        + Send
        + 'static,
{
    let (guard, result) = crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            let Some(guard) = pool
                .checkout_shared_reader_transaction(|| scope.should_stop())
                .map_err(|error| StorageError::driver(StorageCapability::Sql, operation, error))?
            else {
                return Err(StorageError::Timeout {
                    operation: operation.into(),
                });
            };
            let result = query(scope, guard.conn(), false, true);
            if scope.cleanup_failed() {
                guard.poison();
            }
            Ok((guard, result))
        },
    )
    .await?;
    finish_pool_backed_reader_step(transaction, guard, true, operation, result)
}

/// Route one raw-SQL call through the in-memory backend's `PoolBackedReader`.
/// An ordinary read with no open span falls through to the regular per-call
/// pooled checkout ([`run_pool_reader_query`]); every other combination
/// (opening, continuing, or closing the explicit deferred read-transaction
/// span) is handled here so the span retains one connection end to end.
#[allow(clippy::too_many_lines)]
async fn run_pool_backed_reader_query<T, F>(
    transaction: &mut Option<SharedReaderTransactionGuard>,
    pool: Arc<ConnectionPool>,
    operation: &'static str,
    transaction_control: Option<CachedReadTransactionControl>,
    query: F,
) -> khive_storage::types::StorageResult<T>
where
    T: Send + 'static,
    F: FnOnce(
            &crate::read_cancellation::InterruptibleReadScope,
            &rusqlite::Connection,
            bool,
            bool,
        ) -> khive_storage::types::StorageResult<T>
        + Send
        + 'static,
{
    if transaction.is_none() {
        return match transaction_control {
            None => {
                run_pool_reader_query(pool, operation, move |scope, conn| {
                    query(scope, conn, false, true)
                })
                .await
            }
            Some(CachedReadTransactionControl::Finish(keyword))
            | Some(CachedReadTransactionControl::Unsupported(keyword)) => {
                Err(StorageError::InvalidInput {
                    capability: StorageCapability::Sql,
                    operation: operation.into(),
                    message: format!(
                        "pool-backed reader has no admitted transaction for transaction \
                         control ({keyword})"
                    ),
                })
            }
            Some(CachedReadTransactionControl::BeginDeferred) => {
                open_pool_backed_reader_transaction(transaction, pool, operation, query).await
            }
        };
    }

    match transaction_control {
        Some(CachedReadTransactionControl::BeginDeferred) => {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: operation.into(),
                message: "pool-backed reader already owns an admitted read transaction; \
                          nested BEGIN is not supported"
                    .into(),
            });
        }
        Some(CachedReadTransactionControl::Unsupported(keyword)) => {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: operation.into(),
                message: format!(
                    "pool-backed reader's admitted read transaction does not support nested \
                     or write-locking transaction control ({keyword})"
                ),
            });
        }
        None | Some(CachedReadTransactionControl::Finish(_)) => {}
    }

    let expect_open_after = transaction_control.is_none();
    let guard = transaction.take().expect("checked Some above");
    let (guard, result) = crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            let result = query(scope, guard.conn(), true, true);
            if scope.cleanup_failed() {
                guard.poison();
            }
            Ok((guard, result))
        },
    )
    .await?;
    finish_pool_backed_reader_step(transaction, guard, expect_open_after, operation, result)
}

#[async_trait]
impl khive_storage::SqlReader for PoolBackedReader {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "pool_reader.query_row")?;
        let pool = Arc::clone(&self.pool);
        run_pool_backed_reader_query(
            &mut self.transaction,
            pool,
            "pool_reader.query_row",
            transaction_control,
            move |scope, conn, rollback, interruptible| {
                execute_query_row_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "pool_reader.query_row",
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
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "pool_reader.query_all")?;
        let pool = Arc::clone(&self.pool);
        run_pool_backed_reader_query(
            &mut self.transaction,
            pool,
            "pool_reader.query_all",
            transaction_control,
            move |scope, conn, rollback, interruptible| {
                execute_query_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "pool_reader.query_all",
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
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "pool_reader.query_page")?;
        let pool = Arc::clone(&self.pool);
        run_pool_backed_reader_query(
            &mut self.transaction,
            pool,
            "pool_reader.query_page",
            transaction_control,
            move |scope, conn, rollback, interruptible| {
                execute_query_page_interruptibly(
                    scope,
                    conn,
                    &statement,
                    &page,
                    "pool_reader.query_page",
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
        let row = self.query_row(statement).await?;
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
        self.query_all(explain_stmt).await
    }
}

pub(super) struct PoolBackedWriter {
    pub(super) pool: Arc<ConnectionPool>,
}

#[async_trait]
impl khive_storage::SqlReader for PoolBackedWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        run_pool_writer_query(
            pool,
            "pool_writer.query_row",
            writer_query_requires_admission(&statement),
            move |scope, conn, interruptible| {
                execute_query_row_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "pool_writer.query_row",
                    false,
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
        let pool = Arc::clone(&self.pool);
        run_pool_writer_query(
            pool,
            "pool_writer.query_all",
            writer_query_requires_admission(&statement),
            move |scope, conn, interruptible| {
                execute_query_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "pool_writer.query_all",
                    false,
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
        let pool = Arc::clone(&self.pool);
        run_pool_writer_query(
            pool,
            "pool_writer.query_page",
            writer_query_requires_admission(&statement),
            move |scope, conn, interruptible| {
                execute_query_page_interruptibly(
                    scope,
                    conn,
                    &statement,
                    &page,
                    "pool_writer.query_page",
                    false,
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
impl khive_storage::SqlWriter for PoolBackedWriter {
    async fn execute(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<u64> {
        // Each call checks the pooled writer out and releases it before
        // returning, so a transaction opened here could not outlive the call:
        // the guard would settle it on release while the caller believed it
        // open. Transaction control is refused before anything runs; a
        // transaction is opened through `atomic_unit`, which holds one guard
        // for the whole unit.
        if let Some(keyword) = transaction_control_head(&statement.sql) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: "pool_writer.execute".into(),
                message: format!(
                    "statement is transaction control ({keyword}); a pooled writer is \
                     released after every call, so it cannot hold a transaction across \
                     calls — use atomic_unit to run statements as one transaction"
                ),
            });
        }
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.autocommit_write_unit().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute", e)
            })?;
            let result = (|| {
                let mut stmt = prepare_cached_sql_statement(guard.conn(), &statement.sql)
                    .map_err(|e| map_write_err(e, "pool_writer.execute"))?;
                bind_params(&mut stmt, &statement.params)
                    .map_err(|e| map_write_err(e, "pool_writer.execute"))?;
                let rows = stmt
                    .raw_execute()
                    .map_err(|e| map_write_err(e, "pool_writer.execute"))?;
                Ok(rows as u64)
            })();
            settle_pooled_call(&guard, "pool_writer.execute", result)
                .inspect_err(|error| pool.record_direct_writer_error(error))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_writer.execute", e))?
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::types::StorageResult<u64> {
        // Same all-or-nothing contract as the file-backed path: this batch
        // wraps its list in its own `BEGIN IMMEDIATE`, so reject caller
        // transaction-control statements before executing anything.
        reject_transaction_control_statements(&statements, "pool_writer.execute_batch")?;
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let mut tx_handle = None;
            let guard = pool.writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_batch", e)
            })?;
            let result = (|| {
                let prepared = prepare_batch_statements(guard.conn(), &statements)
                    .map_err(|e| map_write_err(e, "pool_writer.execute_batch"))?;
                guard.conn().execute_batch("BEGIN IMMEDIATE").map_err(|e| {
                    map_write_boundary_err(
                        e,
                        "pool_writer.execute_batch",
                        khive_storage::error::SqliteWriteStage::Begin,
                    )
                })?;
                if let Err(refused) = pool.write_admission().check() {
                    guard
                        .rollback_or_retire("pooled batch capacity admission")
                        .map_err(|error| {
                            error.into_storage_error(
                                StorageCapability::Sql,
                                "pool_writer.execute_batch",
                            )
                        })?;
                    return Err(refused
                        .into_storage_error(StorageCapability::Sql, "pool_writer.execute_batch"));
                }
                let _ = tx_handle.insert(khive_storage::tx_registry::register_scoped(
                    Some("pool_writer.execute_batch".to_string()),
                    pool.origin(),
                ));
                let result = execute_prepared_batch(guard.conn(), prepared, &statements, None)
                    .map_err(|e| map_write_err(e, "pool_writer.execute_batch"));
                match result {
                    Ok(total) => {
                        if let Err(e) = guard.conn().execute_batch("COMMIT") {
                            let unsettled =
                                !crate::connection_settlement::rollback_proves_autocommit(
                                    guard.conn(),
                                );
                            let error = map_write_boundary_err(
                                e,
                                "pool_writer.execute_batch",
                                khive_storage::error::SqliteWriteStage::Commit,
                            );
                            Err(if unsettled {
                                crate::error::unknown_write_settlement(error)
                            } else {
                                error
                            })
                        } else {
                            Ok(total)
                        }
                    }
                    Err(e) => {
                        let unsettled =
                            !crate::connection_settlement::rollback_proves_autocommit(guard.conn());
                        Err(if unsettled {
                            crate::error::unknown_write_settlement(e)
                        } else {
                            e
                        })
                    }
                }
            })();
            result.inspect_err(|error| pool.record_direct_writer_error(error))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_writer.execute_batch", e))?
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        // Boundary: raw scripts are internal/migration-only and do not inherit
        // `execute_batch`'s transaction-control rejection. A script may open
        // and close its own transaction; one it leaves open is rolled back and
        // refused before the pooled writer is released.
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.autocommit_write_unit().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_script", e)
            })?;
            let result = guard
                .conn()
                .execute_batch(&script)
                .map_err(|e| map_write_err(e, "pool_writer.execute_script"));
            settle_pooled_call(&guard, "pool_writer.execute_script", result)
                .inspect_err(|error| pool.record_direct_writer_error(error))
        })
        .await
        .map_err(|e| {
            StorageError::driver(StorageCapability::Sql, "pool_writer.execute_script", e)
        })?
    }
}
