use super::inline_bridge::{block_on_sync, InlineWriter};
use super::*;

/// Keep the complete manual transaction inside one blocking operation, so
/// dropping its async observer cannot release its lease or registry evidence.
pub(super) async fn run_manual_atomic_unit(
    pool: Arc<ConnectionPool>,
    handle_slot: OwnedSemaphorePermit,
    op: AtomicUnitOp,
    event_rows: Arc<AtomicEventRows>,
) -> khive_storage::types::StorageResult<Box<dyn Any + Send>> {
    #[cfg(test)]
    let test_context = super::manual_atomic_owner_tests::current_test_context();

    tokio::task::spawn_blocking(move || {
        // Reverse local-drop order settles/closes the unit before either
        // admission or its registered transaction evidence is released.
        let _handle_slot = handle_slot;
        let mut tx_handle = None;
        let unit = pool.standalone_sql_bridge_transaction_write_unit()?;
        let _ = tx_handle.insert(khive_storage::tx_registry::register_scoped(
            Some("atomic_unit".to_string()),
            pool.origin(),
        ));
        #[cfg(test)]
        if let Some(context) = test_context {
            context.before_body(&pool, unit.conn())?;
        }
        let db = crate::timeout_sink::db_label(&pool);
        let (result, _) = crate::writer_task::execute_wrapped_sql_bridge_transaction(
            unit.conn(),
            "execute",
            &db,
            |conn| {
                let mut inline = InlineWriter {
                    event_rows: Some(event_rows),
                    conn: conn as *const rusqlite::Connection,
                    standalone_db: Some(db.clone()),
                };
                block_on_sync(op(&mut inline)).and_then(|result| result)
            },
        );
        match result {
            Err(StorageError::WriterTaskRequestFailed {
                request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                source,
            }) => Err(*source),
            result => result,
        }
    })
    .await
    .map_err(|error| StorageError::driver(StorageCapability::Sql, "atomic_unit", error))?
}
