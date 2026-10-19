//! Call-scoped ownership for SQL that can write on a cached connection.

use khive_storage::types::StorageResult;

use super::{
    acquire_reader_handle_slot, acquire_standalone_lease, execute_standalone_read,
    reader_capability_admits, Arc, ConnectionPool, SlotTimeoutClass, SqlStatement, SqliteError,
    StandaloneHandle, StorageCapability, StorageError,
};

/// Keep the native connection ahead of its lease on every exit, including
/// unwinding or abandonment of the asynchronous caller. A reusable handle is
/// returned only after the call has proved settlement.
pub(super) struct StandaloneCallOwner {
    handle: Option<StandaloneHandle>,
    pool: Arc<ConnectionPool>,
    _volume_lease: Option<crate::disk_guard::VolumeLease>,
}

impl StandaloneCallOwner {
    pub(super) fn new(handle: StandaloneHandle, pool: Arc<ConnectionPool>) -> Self {
        Self {
            handle: Some(handle),
            pool,
            _volume_lease: None,
        }
    }

    pub(super) fn conn(&self) -> &rusqlite::Connection {
        &self.handle.as_ref().expect("call owns its connection").conn
    }

    pub(super) fn admit(&mut self, operation: &'static str) -> StorageResult<()> {
        self._volume_lease = acquire_standalone_lease(&self.pool, operation)?;
        self.pool
            .write_admission()
            .check()
            .map_err(|error| error.into_storage_error(StorageCapability::Sql, operation))
    }

    /// Whether a transaction was left open. Owned close is an existing valid
    /// settlement proof when the first rollback fails; only a failure to prove
    /// settlement during retirement produces `WriterSettlementUnknown`.
    pub(super) fn settle(&mut self) -> Result<bool, SqliteError> {
        if self.conn().is_autocommit() {
            return Ok(false);
        }
        if self.conn().execute_batch("ROLLBACK").is_ok() && self.conn().is_autocommit() {
            return Ok(true);
        }
        self.retire()?;
        Ok(true)
    }

    pub(super) fn retire(&mut self) -> Result<(), SqliteError> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let StandaloneHandle {
            conn,
            _retained_slot,
            read_transaction_slot,
        } = handle;
        let settled = self.pool.write_admission().close_retired_connection(conn);
        drop((_retained_slot, read_transaction_slot));
        settled
    }

    pub(super) fn into_handle(mut self) -> Option<StandaloneHandle> {
        self.handle.take()
    }
}

impl Drop for StandaloneCallOwner {
    fn drop(&mut self) {
        if self.handle.is_some() && !self.conn().is_autocommit() {
            // The asynchronous waiter can disappear while SQLite is running.
            // Its blocking owner still closes the connection under the lease.
            let _ = self.retire();
        }
        drop(self.handle.take());
    }
}

/// Use the existing conservative reader-capability grammar as an exemption,
/// not as a restriction on writer SQL. Everything else (including transaction
/// control and RETURNING) gets a write owner before preparation or stepping.
pub(super) fn writer_query_requires_admission(statement: &SqlStatement) -> bool {
    reader_capability_admits(&statement.sql).is_err()
}

pub(super) async fn execute_standalone_writer_query<R, F>(
    handle: &mut Option<StandaloneHandle>,
    pool: Arc<ConnectionPool>,
    operation: &'static str,
    requires_admission: bool,
    query: F,
) -> StorageResult<R>
where
    R: Send + 'static,
    F: FnOnce(
            &crate::read_cancellation::InterruptibleReadScope,
            &rusqlite::Connection,
            bool,
            bool,
        ) -> StorageResult<R>
        + Send
        + 'static,
{
    let owned_transaction = handle
        .as_ref()
        .is_some_and(|handle| !handle.conn.is_autocommit());
    if !requires_admission && !owned_transaction {
        return execute_standalone_read(handle, pool, operation, None, query).await;
    }
    if handle.is_none() {
        return Err(StorageError::Pool {
            operation: operation.into(),
            message: "connection already consumed".into(),
        });
    }
    let acquire_slot =
        acquire_reader_handle_slot(&pool, operation, SlotTimeoutClass::ReaderContract);
    let operation_slot = if owned_transaction {
        acquire_slot.await?
    } else {
        crate::await_request_read_phase(operation, acquire_slot).await??
    };
    let owned_handle = handle.take().expect("checked standalone handle");
    let (retained, result) = crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            // Declare this after the operation permit: native settlement and
            // close precede release of both admission budgets on unwind.
            let _operation_slot = operation_slot;
            let mut owner = StandaloneCallOwner::new(owned_handle, pool);
            let result = owner
                .admit(operation)
                .and_then(|()| query(scope, owner.conn(), false, owner.conn().is_autocommit()));
            let settled = owner.settle();
            let result = match settled {
                Err(settlement) => {
                    let settlement = match &result {
                        Err(original) => SqliteError::settlement_with_cause(
                            original,
                            khive_storage::error::SqliteWriteStage::Statement,
                        ),
                        Ok(_) => settlement,
                    };
                    Err(settlement.into_storage_error(StorageCapability::Sql, operation))
                }
                Ok(true) => result.and_then(|_| {
                    Err(StorageError::InvalidInput {
                        capability: StorageCapability::Sql,
                        operation: operation.into(),
                        message: "the call left a transaction open; it was rolled back before \
                                  the volume lease was released — use atomic_unit to run \
                                  statements as one transaction"
                            .into(),
                    })
                }),
                Ok(false) => result,
            };
            // A failed callback cleanup must never restore this connection.
            let result = if scope.cleanup_failed() {
                owner
                    .retire()
                    .map_err(|error| error.into_storage_error(StorageCapability::Sql, operation))
                    .and(result)
            } else {
                result
            };
            Ok((owner.into_handle(), result))
        },
    )
    .await?;
    *handle = retained;
    result
}
