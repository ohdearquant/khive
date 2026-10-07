use super::*;

pub(super) fn run_graph_mutation_transaction<R, F>(
    pool: &ConnectionPool,
    conn: &rusqlite::Connection,
    pooled: bool,
    operation: F,
) -> StorageResult<R>
where
    F: FnOnce(&rusqlite::Connection) -> StorageResult<R>,
{
    if !conn.is_autocommit() {
        if pooled {
            pool.retire_pooled_writer(conn);
        }
        return Err(StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        });
    }
    if let Err(error) = conn.execute_batch("BEGIN IMMEDIATE") {
        if !conn.is_autocommit() {
            if pooled {
                pool.retire_pooled_writer(conn);
            }
            return Err(StorageError::WriterTaskTerminated {
                request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
            });
        }
        crate::timeout_sink::maybe_emit_busy(
            &crate::timeout_sink::db_label(pool),
            crate::timeout_sink::Site::StandaloneGraph,
            &error,
        );
        return Err(map_err(error, GRAPH_MUTATION_EVENTS_OP))
            .inspect_err(|error| pool.record_direct_writer_error(error));
    }
    if let Err(error) = pool.write_admission().check() {
        if conn.execute_batch("ROLLBACK").is_err() || !conn.is_autocommit() {
            if pooled {
                pool.retire_pooled_writer(conn);
            }
            return Err(StorageError::WriterTaskTerminated {
                request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
            });
        }
        return Err(map_sqlite_err(error, GRAPH_MUTATION_EVENTS_OP));
    }
    let _tx_handle = khive_storage::tx_registry::register_scoped(
        Some(GRAPH_MUTATION_EVENTS_OP.to_string()),
        pool.origin(),
    );
    let (result, terminal_state) = crate::writer_task::execute_wrapped_transaction(
        conn,
        "compose_graph_mutation_events.commit",
        operation,
    );
    if pooled && terminal_state.is_some() {
        pool.retire_pooled_writer(conn);
    }
    result.inspect_err(|error| pool.record_direct_writer_error(error))
}

#[cfg(test)]
#[path = "write_transaction_busy_tests.rs"]
mod direct_busy_tests;
