//! File-backed readers and their explicit transaction handles.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use khive_storage::error::StorageError;
use khive_storage::types::{PageRequest, SqlRow, SqlStatement, SqlValue};
use khive_storage::StorageCapability;
use tokio::sync::OwnedSemaphorePermit;

use crate::pool::ConnectionPool;

use super::{
    acquire_reader_handle_slot, admit_reader_capability_sql, cached_read_transaction_control,
    execute_query_interruptibly, execute_query_page_interruptibly, execute_query_row_interruptibly,
    open_standalone_reader_on_blocking, run_pool_reader_query, CachedReadTransactionControl,
    SlotTimeoutClass,
};

// =============================================================================
// File-backed: pooled SqliteReader with an explicit transaction exception
// =============================================================================

pub(super) const CACHED_READ_TRANSACTION_LABEL: &str = "sql_bridge_cached_read_transaction";

/// Admission and observability guards for one explicit cached-reader
/// transaction. Both guards are installed only after SQLite accepts `BEGIN`
/// and are retained together until SQLite reports autocommit again or the
/// owning connection is closed.
///
/// Field order is deliberate: after [`StandaloneHandle::conn`] closes, the
/// reader permit is returned before the registry evidence disappears. There
/// is therefore no interval in which SQLite can still own the snapshot while
/// the transaction is absent from `tx_registry`.
pub(super) struct CachedReadTransaction {
    _slot: OwnedSemaphorePermit,
    _tx_handle: khive_storage::tx_registry::TxHandle,
    /// When this explicit `BEGIN` was admitted. Read on every subsequent
    /// reuse of the owning cached-reader handle (#1846): a transaction whose
    /// age has crossed `read_tx_max_age` is rolled back instead of being
    /// extended by another call, bounding how long any one reader can pin
    /// the WAL snapshot regardless of how many further requests it makes.
    opened_at: Instant,
}

pub(super) struct StandaloneHandle {
    pub(super) conn: rusqlite::Connection,
    /// Present only for a standalone read-write handle, whose one-permit
    /// connection budget remains handle-scoped. Read-only connections exist
    /// only for explicit deferred transactions and retain reader admission in
    /// `read_transaction_slot` until terminal control.
    pub(super) _retained_slot: Option<OwnedSemaphorePermit>,
    /// Present only while a cached read-only connection owns one explicit
    /// multi-call read transaction. Field order is load-bearing: Rust drops
    /// `conn` before these guards, so cancellation or handle drop closes the
    /// SQLite transaction before returning reader admission or deregistering
    /// the transaction span.
    pub(super) read_transaction_slot: Option<CachedReadTransaction>,
}

impl StandaloneHandle {
    /// Whether this is an idle-cacheable read-only connection rather than a
    /// writer connection covered by the handle-scoped writer permit.
    pub(super) fn is_cached_reader(&self) -> bool {
        self._retained_slot.is_none()
    }

    pub(super) fn has_read_transaction(&self) -> bool {
        self.read_transaction_slot.is_some()
    }
}

pub(super) struct SqliteReader {
    /// Present only for the ADR-005/ADR-091 explicitly admitted multi-call
    /// deferred transaction. Ordinary reads never populate this field and
    /// route through `ConnectionPool::reader` for each operation.
    pub(super) handle: Option<StandaloneHandle>,
    pub(super) pool: Arc<ConnectionPool>,
    /// Fail-loud compatibility state after a transaction connection could not
    /// be restored safely. `None` normally means "pooled ordinary route";
    /// this bit distinguishes that from "exceptional connection consumed".
    pub(super) poisoned: bool,
}

pub(super) async fn open_explicit_read_transaction_handle(
    pool: Arc<ConnectionPool>,
) -> khive_storage::types::StorageResult<StandaloneHandle> {
    let open_slot = crate::await_request_read_phase(
        "sql_bridge.reader_open",
        acquire_reader_handle_slot(
            &pool,
            "sql_bridge.reader_open",
            SlotTimeoutClass::ReaderContract,
        ),
    )
    .await??;
    let (conn, open_slot) = crate::await_request_read_phase(
        "sql_bridge.reader_open",
        open_standalone_reader_on_blocking(pool, open_slot),
    )
    .await??;
    drop(open_slot);
    Ok(StandaloneHandle {
        conn,
        _retained_slot: None,
        read_transaction_slot: None,
    })
}

impl SqliteReader {
    /// Select the standalone path only for a live or newly requested explicit
    /// deferred transaction. `false` means the caller must execute this
    /// operation through the bounded reader pool.
    async fn use_explicit_transaction_handle(
        &mut self,
        transaction_control: Option<CachedReadTransactionControl>,
        operation: &'static str,
    ) -> khive_storage::types::StorageResult<bool> {
        if self.poisoned {
            return Err(StorageError::Pool {
                operation: operation.into(),
                message: "connection already consumed".into(),
            });
        }
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

    /// A standalone connection exists only for one explicit transaction.
    /// Close it immediately after a failed BEGIN, COMMIT/ROLLBACK, age
    /// eviction, or cancellation cleanup restores autocommit. A later
    /// ordinary query must return to pooled routing instead of silently
    /// retaining a standalone cache.
    fn close_inactive_transaction_handle(&mut self) {
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_cached_reader() && !handle.has_read_transaction())
        {
            drop(self.handle.take());
        }
    }
}

/// Run one file-backed read while coupling its connection state to the active
/// reader permit.
///
/// This path serves only the explicit deferred read-transaction exception and
/// reader-supertrait calls on a standalone read-write handle. Ordinary
/// read-only traffic uses [`run_pool_reader_query`]. A successful top-level
/// deferred `BEGIN` moves its operation permit into the handle; subsequent
/// reads reuse it until `COMMIT`/`END`/`ROLLBACK` restores autocommit. The
/// connection is declared before that retained permit, so dropping or
/// cancelling the handle closes SQLite first and releases admission second.
/// A standalone writer's reader calls still take active-reader admission; once
/// its connection is outside autocommit, the acquisition and SELECT are
/// completion-preserving rather than request-cancellable.
pub(super) async fn execute_standalone_read<R, F>(
    handle: &mut Option<StandaloneHandle>,
    pool: Arc<ConnectionPool>,
    operation: &'static str,
    transaction_control: Option<CachedReadTransactionControl>,
    read: F,
) -> khive_storage::types::StorageResult<R>
where
    R: Send + 'static,
    F: FnOnce(
            &crate::read_cancellation::InterruptibleReadScope,
            &rusqlite::Connection,
            bool,
            bool,
        ) -> khive_storage::types::StorageResult<R>
        + Send
        + 'static,
{
    if handle.is_none() {
        return Err(StorageError::Pool {
            operation: operation.into(),
            message: "connection already consumed".into(),
        });
    }
    let active_read_transaction = handle
        .as_ref()
        .is_some_and(|handle| handle.is_cached_reader() && handle.has_read_transaction());
    let completion_preserving_writer_transaction = handle
        .as_ref()
        .is_some_and(|handle| !handle.is_cached_reader() && !handle.conn.is_autocommit());
    let mut operation_slot = if active_read_transaction {
        None
    } else if completion_preserving_writer_transaction {
        // A read inside an admitted write transaction still counts against the
        // reader budget, but request cancellation cannot skip that admission
        // and strand the transaction between statements.
        Some(acquire_reader_handle_slot(&pool, operation, SlotTimeoutClass::ReaderContract).await?)
    } else {
        Some(
            crate::await_request_read_phase(
                operation,
                acquire_reader_handle_slot(&pool, operation, SlotTimeoutClass::ReaderContract),
            )
            .await??,
        )
    };
    let Some(owned_handle) = handle.take() else {
        return Err(StorageError::Pool {
            operation: operation.into(),
            message: "connection already consumed".into(),
        });
    };
    let origin = pool.origin();
    let read_tx_max_age = pool.config().read_tx_max_age;
    let (owned_handle, result) = crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            let mut owned_handle = owned_handle;
            let cached_reader = owned_handle.is_cached_reader();
            let entered_with_transaction = owned_handle.has_read_transaction();
            let entered_autocommit = owned_handle.conn.is_autocommit();
            let mut restore_handle = true;
            let mut result = if cached_reader && entered_with_transaction && entered_autocommit {
                // A connection cannot pin a snapshot in autocommit. Repair the
                // admission state before returning the invariant failure.
                drop(owned_handle.read_transaction_slot.take());
                Err(StorageError::InvalidInput {
                    capability: StorageCapability::Sql,
                    operation: operation.into(),
                    message: "cached read-only handle retained transaction admission after SQLite \
                          had already returned to autocommit; the stale permit was released"
                        .into(),
                })
            } else if cached_reader && !entered_with_transaction && !entered_autocommit {
                Err(StorageError::InvalidInput {
                    capability: StorageCapability::Sql,
                    operation: operation.into(),
                    message: "cached read-only handle entered the operation outside autocommit; \
                          its transaction was rolled back before releasing the reader permit"
                        .into(),
                })
            } else if cached_reader
                && entered_with_transaction
                && owned_handle
                    .read_transaction_slot
                    .as_ref()
                    .is_some_and(|tx| tx.opened_at.elapsed() >= read_tx_max_age)
            {
                // #1846: this handle's admitted read transaction has pinned a
                // WAL snapshot for at least `read_tx_max_age` — reject the
                // continuation and roll it back instead of extending the pin
                // for another call, regardless of what the caller asked for.
                crate::checkpoint::note_read_tx_max_age_eviction();
                match owned_handle.conn.execute_batch("ROLLBACK") {
                    Ok(()) if owned_handle.conn.is_autocommit() => {
                        drop(owned_handle.read_transaction_slot.take());
                        Err(StorageError::ReadTransactionAgeEvicted {
                            operation: operation.into(),
                            max_age_secs: read_tx_max_age.as_secs(),
                        })
                    }
                    Ok(()) => {
                        restore_handle = false;
                        Err(StorageError::ReadTransactionAgeEvictionCleanupFailed {
                            operation: operation.into(),
                            max_age_secs: read_tx_max_age.as_secs(),
                            message: "rollback did not restore autocommit".into(),
                        })
                    }
                    Err(error) => {
                        restore_handle = false;
                        Err(StorageError::ReadTransactionAgeEvictionCleanupFailed {
                            operation: operation.into(),
                            max_age_secs: read_tx_max_age.as_secs(),
                            message: format!("rollback failed: {error}"),
                        })
                    }
                }
            } else if cached_reader && entered_with_transaction {
                match transaction_control {
                    None | Some(CachedReadTransactionControl::Finish(_)) => {
                        read(scope, &owned_handle.conn, true, true)
                    }
                    Some(CachedReadTransactionControl::BeginDeferred) => {
                        Err(StorageError::InvalidInput {
                            capability: StorageCapability::Sql,
                            operation: operation.into(),
                            message: "cached read-only handle already owns an admitted read \
                                  transaction; nested BEGIN is not supported"
                                .into(),
                        })
                    }
                    Some(CachedReadTransactionControl::Unsupported(keyword)) => {
                        Err(StorageError::InvalidInput {
                            capability: StorageCapability::Sql,
                            operation: operation.into(),
                            message: format!(
                                "cached read-only transaction does not support nested or \
                             write-locking transaction control ({keyword})"
                            ),
                        })
                    }
                }
            } else if cached_reader {
                match transaction_control {
                    None | Some(CachedReadTransactionControl::BeginDeferred) => {
                        read(scope, &owned_handle.conn, false, true)
                    }
                    Some(CachedReadTransactionControl::Finish(keyword))
                    | Some(CachedReadTransactionControl::Unsupported(keyword)) => {
                        Err(StorageError::InvalidInput {
                            capability: StorageCapability::Sql,
                            operation: operation.into(),
                            message: format!(
                                "cached read-only handle has no admitted transaction for \
                             transaction control ({keyword})"
                            ),
                        })
                    }
                }
            } else {
                read(scope, &owned_handle.conn, false, entered_autocommit)
            };

            if scope.cleanup_failed() {
                // A connection-global callback that could not be removed may
                // fire for an unrelated future borrower. Closing this handle
                // is the only safe recovery; its transaction, if any, ends
                // before reader admission is released below.
                restore_handle = false;
            }

            // An interrupted explicit read transaction must never be restored to
            // the cached handle: it may still own a WAL snapshot and SQLite's
            // interrupted flag applies to the transaction as a whole. Roll back
            // before releasing its retained admission; if rollback cannot prove
            // autocommit, discard the connection.
            if cached_reader
                && matches!(result, Err(StorageError::Timeout { .. }))
                && !owned_handle.conn.is_autocommit()
            {
                match owned_handle.conn.execute_batch("ROLLBACK") {
                    Ok(()) if owned_handle.conn.is_autocommit() => {
                        drop(owned_handle.read_transaction_slot.take());
                    }
                    Ok(()) => {
                        restore_handle = false;
                        result = Err(StorageError::Transaction {
                            operation: operation.into(),
                            message:
                                "interrupted read transaction rollback did not restore autocommit; \
                                  the connection was discarded"
                                    .into(),
                        });
                    }
                    Err(error) => {
                        restore_handle = false;
                        result = Err(StorageError::Transaction {
                            operation: operation.into(),
                            message: format!(
                                "failed to roll back interrupted read transaction ({error}); \
                             the connection was discarded"
                            ),
                        });
                    }
                }
            }

            if cached_reader && entered_with_transaction {
                if owned_handle.conn.is_autocommit() {
                    // SQLite has ended the snapshot; release only after observing
                    // that terminal state. This also fails closed if an ordinary
                    // statement unexpectedly ended the transaction.
                    drop(owned_handle.read_transaction_slot.take());
                    if result.is_ok()
                        && !matches!(
                            transaction_control,
                            Some(CachedReadTransactionControl::Finish(_))
                        )
                    {
                        result = Err(StorageError::InvalidInput {
                            capability: StorageCapability::Sql,
                            operation: operation.into(),
                            message: "cached read-only operation unexpectedly ended its admitted \
                                  transaction; reader admission was released after autocommit"
                                .into(),
                        });
                    }
                } else if result.is_ok()
                    && matches!(
                        transaction_control,
                        Some(CachedReadTransactionControl::Finish(_))
                    )
                {
                    result = Err(StorageError::InvalidInput {
                        capability: StorageCapability::Sql,
                        operation: operation.into(),
                        message: "transaction-ending control completed but the cached reader \
                              remained outside autocommit; its reader permit remains retained"
                            .into(),
                    });
                }
            } else if cached_reader
                && entered_autocommit
                && matches!(
                    transaction_control,
                    Some(CachedReadTransactionControl::BeginDeferred)
                )
                && result.is_ok()
            {
                if owned_handle.conn.is_autocommit() {
                    result = Err(StorageError::InvalidInput {
                        capability: StorageCapability::Sql,
                        operation: operation.into(),
                        message: "deferred BEGIN completed without opening a read transaction"
                            .into(),
                    });
                } else {
                    match operation_slot.take() {
                        Some(slot) => {
                            let tx_handle = khive_storage::tx_registry::register_scoped(
                                Some(CACHED_READ_TRANSACTION_LABEL.to_string()),
                                origin.clone(),
                            );
                            owned_handle.read_transaction_slot = Some(CachedReadTransaction {
                                _slot: slot,
                                _tx_handle: tx_handle,
                                opened_at: Instant::now(),
                            });
                        }
                        None => {
                            result = Err(StorageError::Pool {
                                operation: operation.into(),
                                message: "successful cached-reader BEGIN had no operation permit; \
                                      its transaction was rolled back before returning"
                                    .into(),
                            });
                        }
                    }
                }
            }

            // Any non-autocommit state without its retained admission is stale or
            // was opened by a statement the transaction classifier did not admit.
            if cached_reader
                && owned_handle.read_transaction_slot.is_none()
                && !owned_handle.conn.is_autocommit()
            {
                match owned_handle.conn.execute_batch("ROLLBACK") {
                    Ok(()) if owned_handle.conn.is_autocommit() => {
                        if result.is_ok() {
                            result = Err(StorageError::InvalidInput {
                                capability: StorageCapability::Sql,
                                operation: operation.into(),
                                message: "cached read-only operation left the connection outside \
                                      autocommit; its transaction was rolled back before \
                                      releasing the reader permit"
                                    .into(),
                            });
                        }
                    }
                    Ok(()) => {
                        restore_handle = false;
                        result = Err(StorageError::Transaction {
                            operation: operation.into(),
                            message: "ROLLBACK completed but the cached reader remained outside \
                                  autocommit; the connection was discarded before releasing \
                                  the reader permit"
                                .into(),
                        });
                    }
                    Err(error) => {
                        restore_handle = false;
                        result = Err(StorageError::Transaction {
                            operation: operation.into(),
                            message: format!(
                            "failed to roll back a cached reader outside autocommit ({error}); \
                             the connection was discarded before releasing the reader permit"
                        ),
                        });
                    }
                }
            }

            let owned_handle = if restore_handle {
                Some(owned_handle)
            } else {
                // Closing the poisoned connection ends any remaining transaction.
                // This must precede the active-reader permit release below.
                drop(owned_handle);
                None
            };
            // For ordinary reads and rejected controls this is the operation
            // permit. A successful BEGIN moved it into `owned_handle`; poisoned
            // handles were closed above before this remaining permit is released.
            drop(operation_slot);
            Ok((owned_handle, result))
        },
    )
    .await?;
    *handle = owned_handle;
    result
}

#[async_trait]
impl khive_storage::SqlReader for SqliteReader {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "query_row")?;
        if !self
            .use_explicit_transaction_handle(transaction_control, "query_row")
            .await?
        {
            return run_pool_reader_query(
                Arc::clone(&self.pool),
                "query_row",
                move |scope, conn| {
                    execute_query_row_interruptibly(
                        scope,
                        conn,
                        &statement,
                        "query_row",
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
            "query_row",
            transaction_control,
            move |scope, conn, rollback, interruptible| {
                execute_query_row_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "query_row",
                    rollback,
                    interruptible,
                )
            },
        )
        .await;
        if self.handle.is_none() {
            self.poisoned = true;
        }
        self.close_inactive_transaction_handle();
        result
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "query_all")?;
        if !self
            .use_explicit_transaction_handle(transaction_control, "query_all")
            .await?
        {
            return run_pool_reader_query(
                Arc::clone(&self.pool),
                "query_all",
                move |scope, conn| {
                    execute_query_interruptibly(scope, conn, &statement, "query_all", false, true)
                },
            )
            .await;
        }
        let result = execute_standalone_read(
            &mut self.handle,
            Arc::clone(&self.pool),
            "query_all",
            transaction_control,
            move |scope, conn, rollback, interruptible| {
                execute_query_interruptibly(
                    scope,
                    conn,
                    &statement,
                    "query_all",
                    rollback,
                    interruptible,
                )
            },
        )
        .await;
        if self.handle.is_none() {
            self.poisoned = true;
        }
        self.close_inactive_transaction_handle();
        result
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "query_page")?;
        if !self
            .use_explicit_transaction_handle(transaction_control, "query_page")
            .await?
        {
            return run_pool_reader_query(
                Arc::clone(&self.pool),
                "query_page",
                move |scope, conn| {
                    execute_query_page_interruptibly(
                        scope,
                        conn,
                        &statement,
                        &page,
                        "query_page",
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
            "query_page",
            transaction_control,
            move |scope, conn, rollback, interruptible| {
                execute_query_page_interruptibly(
                    scope,
                    conn,
                    &statement,
                    &page,
                    "query_page",
                    rollback,
                    interruptible,
                )
            },
        )
        .await;
        if self.handle.is_none() {
            self.poisoned = true;
        }
        self.close_inactive_transaction_handle();
        result
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
