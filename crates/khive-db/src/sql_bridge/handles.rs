use super::{
    map_rusqlite_err, Arc, ConnectionPool, OwnedSemaphorePermit, Semaphore,
    StandaloneReaderPurpose, StorageCapability, StorageError,
};

/// How an elapsed handle-slot deadline is classified. ADR-005 pins the closed
/// raw-SQL standalone exception (`sql_bridge.reader_open`) and reads on a
/// standalone writer to `StorageError::Timeout`; ordinary reader-pool
/// saturation reports the typed `AdmissionTimeout` through
/// `ConnectionPool::resolve_reader_checkout`.
///
/// Classification is decided by this enum and never by the operation label.
/// The label names the caller's own read (`query_row`, `writer.query_all`, and
/// so on) so that a recorded maximum reader hold can be attributed to the read
/// that caused it.
#[derive(Clone, Copy)]
pub(super) enum SlotTimeoutClass {
    Admission,
    ReaderContract,
}

pub(super) async fn acquire_reader_handle_slot(
    pool: &ConnectionPool,
    operation: &'static str,
    class: SlotTimeoutClass,
) -> Result<OwnedSemaphorePermit, StorageError> {
    let result = acquire_handle_slot(
        pool.sql_bridge_reader_slots(),
        pool.config().checkout_timeout,
        operation,
        class,
    )
    .await;
    if matches!(
        &result,
        Err(StorageError::Timeout { .. } | StorageError::AdmissionTimeout { .. })
    ) {
        pool.record_reader_admission_timeout();
    }
    result
}

/// Serialize whole units on the one shared in-memory connection. Event-store
/// transactions participate in the same budget as raw SQL atomic units.
/// The permit must outlive the unit's final COMMIT or ROLLBACK.
pub(crate) async fn acquire_in_memory_write_unit(
    pool: &ConnectionPool,
    operation: &'static str,
) -> Result<OwnedSemaphorePermit, StorageError> {
    acquire_handle_slot(
        pool.sql_bridge_writer_slots(),
        pool.config().checkout_timeout,
        operation,
        SlotTimeoutClass::Admission,
    )
    .await
}

pub(super) async fn acquire_handle_slot(
    slots: Arc<Semaphore>,
    timeout: std::time::Duration,
    operation: &'static str,
    class: SlotTimeoutClass,
) -> Result<OwnedSemaphorePermit, StorageError> {
    tokio::time::timeout(timeout, slots.acquire_owned())
        .await
        .map_err(|_| match class {
            SlotTimeoutClass::Admission => StorageError::AdmissionTimeout {
                operation: operation.into(),
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                pool_identity: None,
            },
            SlotTimeoutClass::ReaderContract => StorageError::Timeout {
                operation: operation.into(),
            },
        })?
        .map_err(|error| StorageError::Pool {
            operation: operation.into(),
            message: error.to_string(),
        })
}

// =============================================================================
// Standalone connection readers/writers (file-backed databases)
// =============================================================================

pub(super) fn open_standalone_reader(
    pool: &ConnectionPool,
) -> Result<rusqlite::Connection, StorageError> {
    pool.open_standalone_reader(StandaloneReaderPurpose::ExplicitSqlReadTransaction)
        .map_err(|error| StorageError::driver(StorageCapability::Sql, "open_reader", error))
}

#[cfg(test)]
pub(super) fn open_external_writer_fixture(
    pool: &ConnectionPool,
) -> Result<rusqlite::Connection, StorageError> {
    let conn = crate::pool::open_external_writer_fixture(pool)
        .map_err(|e| e.into_storage_error(StorageCapability::Sql, "open_writer"))?;
    configure_standalone_writer(pool, conn)
}

pub(crate) fn build_standalone_writer(
    pool: Arc<ConnectionPool>,
    conn: rusqlite::Connection,
    slot: OwnedSemaphorePermit,
) -> Result<Box<dyn khive_storage::SqlWriter>, StorageError> {
    let conn = configure_standalone_writer(&pool, conn)?;
    Ok(Box::new(super::SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(super::StandaloneHandle {
            conn,
            _retained_slot: Some(slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool,
    }))
}

fn configure_standalone_writer(
    pool: &ConnectionPool,
    conn: rusqlite::Connection,
) -> Result<rusqlite::Connection, StorageError> {
    let config = pool.config();
    conn.busy_timeout(config.busy_timeout)
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;
    conn.pragma_update(None, "cache_size", crate::pool::sqlite_cache_size_pragma())
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;
    conn.pragma_update(None, "mmap_size", crate::pool::sqlite_mmap_size_pragma())
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;

    Ok(conn)
}

/// Lift a standalone open onto the blocking thread pool while carrying the
/// connection-cap permit into the same closure.
///
/// If the awaiting future is cancelled, the detached blocking closure owns
/// both the connection result and the permit until it finishes, so the cap
/// cannot be released while an open is still running.
pub(super) async fn open_standalone_on_blocking<F>(
    pool: Arc<ConnectionPool>,
    slot: OwnedSemaphorePermit,
    operation: &'static str,
    open: F,
) -> khive_storage::types::StorageResult<(rusqlite::Connection, OwnedSemaphorePermit)>
where
    F: FnOnce(&ConnectionPool) -> Result<rusqlite::Connection, StorageError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || open(&pool).map(|conn| (conn, slot)))
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, operation, e))?
}

/// [`open_standalone_reader`] lifted onto the blocking thread pool.
///
/// Opening a SQLite connection is filesystem I/O (open the file, read the
/// database header) followed by pragmas executed through SQLite. No
/// database lock is acquired at open itself — locks are taken on the first
/// statement — but filesystem latency is unbounded, and this module already
/// runs every other rusqlite call under `spawn_blocking`, so the open gets
/// the same treatment instead of blocking an async worker thread. The reader
/// permit is supplied to the helper and returned with the connection.
pub(super) async fn open_standalone_reader_on_blocking(
    pool: Arc<ConnectionPool>,
    slot: OwnedSemaphorePermit,
) -> khive_storage::types::StorageResult<(rusqlite::Connection, OwnedSemaphorePermit)> {
    open_standalone_on_blocking(pool, slot, "open_reader", open_standalone_reader).await
}

/// Open the writer handle without a premature capacity sample. Its later
/// operation owns admission; a cold handle must allow a recovery checkpoint.
/// See [`open_standalone_reader_on_blocking`] for the blocking rationale.
pub(super) async fn open_standalone_writer_on_blocking(
    pool: Arc<ConnectionPool>,
    slot: OwnedSemaphorePermit,
) -> khive_storage::types::StorageResult<Box<dyn khive_storage::SqlWriter>> {
    tokio::task::spawn_blocking(move || pool.open_sql_bridge_writer(slot))
        .await
        .map_err(|error| StorageError::driver(StorageCapability::Sql, "open_writer", error))?
}
