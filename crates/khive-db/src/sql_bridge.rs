//! SqlAccess bridge: connects `ConnectionPool` to `khive_storage::SqlAccess`.
//!
//! Two modes:
//! - **File-backed**: ordinary reads check out pooled readers per operation.
//!   The only standalone-reader exception is an explicitly admitted multi-call
//!   deferred read transaction; standalone writer handles remain capped at one.
//!   Cross-statement write atomicity goes through `atomic_unit`, which drives a
//!   single registered raw transaction span rather than a caller-held per-tx
//!   connection.
//! - **Memory**: Uses pool-backed approach (acquire pool connection per-query inside `spawn_blocking`).

#[path = "sql_bridge/write_errors.rs"]
mod write_errors;

#[path = "sql_bridge/manual_atomic.rs"]
mod manual_atomic;
use manual_atomic::run_manual_atomic_unit;
#[path = "sql_bridge/standalone_admission.rs"]
mod standalone_admission;
use standalone_admission::{
    acquire_standalone_lease, admit_standalone_operation, StandaloneWriteError,
};
mod standalone_batch;
#[cfg(test)]
use standalone_batch::BatchPoisonReason;
use standalone_batch::{
    run_standalone_batch, run_standalone_script, run_standalone_statement,
    run_standalone_top_level, BatchFailure, PoisonedBatchError,
};

mod standalone_call;
use standalone_call::{
    execute_standalone_writer_query, writer_query_requires_admission, StandaloneCallOwner,
};

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use khive_storage::error::StorageError;
use khive_storage::types::{PageRequest, SqlColumn, SqlRow, SqlStatement, SqlValue};
use khive_storage::{AtomicUnitOp, StorageCapability, TopLevelMaintenance};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, SharedReaderTransactionGuard, StandaloneReaderPurpose};

// =============================================================================
// Shared helpers
// =============================================================================

mod batch_exec;
use batch_exec::{
    cached_read_transaction_control, execute_prepared_batch, next_sqlite_token,
    reject_transaction_control_statements, skip_sqlite_empty_prefix, transaction_control_head,
    CachedReadTransactionControl,
};

mod rows;

pub(crate) use rows::bind_params;
use rows::{
    prepare_batch_statements, prepare_cached_sql_statement, prepare_sql_statement, row_to_sql_row,
    AtomicEventRows,
};
#[cfg(test)]
use rows::{COUNTED_EVENT_INSERT_LABELS, ROW_CONVERSIONS};

#[cfg(test)]
#[path = "atomic_event_usage_tests.rs"]
mod atomic_event_usage_tests;

mod query_execution;
use query_execution::{
    execute_prepared_query, execute_prepared_query_page, execute_prepared_query_row, execute_query,
    execute_query_page, execute_query_row, map_rusqlite_err, map_write_boundary_err, map_write_err,
    prepare_bound_statement, settle_pooled_call,
};

#[cfg(test)]
#[path = "sql_bridge/sync_executor_tests.rs"]
mod sync_executor_tests;

/// Execute trusted statements inside a caller-owned active transaction.
///
/// The caller owns writer admission, commit, rollback, and connection settlement.
/// This function starts no transaction or savepoint, installs no hooks, retries
/// nothing, and does not account committed events. The returned affected-row sum
/// is execution evidence, not a commit receipt; zero affected rows is successful.
///
/// Explicit transaction-control heads are rejected across the list before
/// preparation. Each element must contain only one SQL statement. Other prepare
/// failures may be deferred until execution to allow earlier schema changes, so
/// an error can follow earlier successful writes. Missing raw parameters bind as
/// NULL; excess parameters fail binding before that element steps.
///
/// This is not a SQL sandbox: PRAGMAs, UDFs, triggers, and virtual tables may have
/// connection or external effects, and SQLite errors can end the transaction.
/// The owner must handle rollback and verify settlement on error.
///
/// # Errors
/// Returns [`SqliteError::InvalidData`] for autocommit connections (even with an
/// empty list) or explicit transaction control. Native prepare, bind, and step
/// failures are preserved as [`SqliteError::Rusqlite`].
pub fn execute_statements(
    conn: &rusqlite::Connection,
    statements: &[SqlStatement],
) -> Result<u64, SqliteError> {
    if conn.is_autocommit() {
        return Err(SqliteError::InvalidData(
            "execute_statements requires a caller-owned active transaction".into(),
        ));
    }
    for (index, statement) in statements.iter().enumerate() {
        if let Some(keyword) = transaction_control_head(&statement.sql) {
            return Err(SqliteError::InvalidData(format!(
                "statement at index {index} is transaction control ({keyword}); \
                 the caller owns the transaction boundary"
            )));
        }
    }
    let prepared = prepare_batch_statements(conn, statements)?;
    Ok(execute_prepared_batch(conn, prepared, statements, None)?)
}

mod reader_capability;
use reader_capability::{
    admit_reader_capability_sql, execute_query_interruptibly, execute_query_page_interruptibly,
    execute_query_row_interruptibly,
};
pub(crate) use reader_capability::{reader_capability_admits, statement_guard_select_admits};

mod handles;
pub(crate) use handles::acquire_in_memory_write_unit;
pub(crate) use handles::build_standalone_writer;
use handles::{
    acquire_handle_slot, acquire_reader_handle_slot, open_standalone_reader_on_blocking,
    open_standalone_writer_on_blocking, SlotTimeoutClass,
};
#[cfg(test)]
use handles::{open_external_writer_fixture, open_standalone_on_blocking, open_standalone_reader};

mod sqlite_reader;
#[cfg(test)]
use sqlite_reader::CACHED_READ_TRANSACTION_LABEL;
use sqlite_reader::{
    execute_standalone_read, open_explicit_read_transaction_handle, SqliteReader, StandaloneHandle,
};

mod sqlite_writer;
use sqlite_writer::{execute_top_level_maintenance, SqliteWriter};

mod pool_backed;
use pool_backed::{run_pool_reader_query, PoolBackedReader, PoolBackedWriter};

mod inline_bridge;
pub use inline_bridge::SqlBridge;
#[cfg(test)]
use inline_bridge::{block_on_sync, InlineWriter};

#[cfg(test)]
#[path = "sql_bridge_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "sql_bridge/direct_busy_tests.rs"]
mod direct_busy_tests;

#[cfg(test)]
mod manual_atomic_owner_tests;

#[cfg(test)]
#[path = "sql_bridge/settlement_hazard_tests.rs"]
mod settlement_hazard_tests;

#[cfg(test)]
mod writer_query_admission_tests;
