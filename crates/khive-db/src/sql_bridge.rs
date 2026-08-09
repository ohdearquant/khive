//! SqlAccess bridge: connects `ConnectionPool` to `khive_storage::SqlAccess`.
//!
//! Two modes:
//! - **File-backed**: Opens standalone connections per reader/writer handle under pool-wide caps.
//!   Cross-statement atomicity goes through `atomic_unit`, which drives a single
//!   registered raw transaction span rather than a caller-held per-tx connection.
//! - **Memory**: Uses pool-backed approach (acquire pool connection per-query inside `spawn_blocking`).

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;

use khive_storage::error::StorageError;
use khive_storage::types::{PageRequest, SqlColumn, SqlRow, SqlStatement, SqlValue};
use khive_storage::{AtomicUnitOp, StorageCapability};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

// =============================================================================
// Shared helpers
// =============================================================================

/// Convert a rusqlite `Row` into an owned `SqlRow`.
fn row_to_sql_row(row: &rusqlite::Row<'_>, col_count: usize, col_names: &[String]) -> SqlRow {
    #[cfg(test)]
    ROW_CONVERSIONS.with(|count| count.set(count.get() + 1));

    let mut columns = Vec::with_capacity(col_count);
    for i in 0..col_count {
        let value = match row.get_ref(i) {
            Ok(rusqlite::types::ValueRef::Null) => SqlValue::Null,
            Ok(rusqlite::types::ValueRef::Integer(v)) => SqlValue::Integer(v),
            Ok(rusqlite::types::ValueRef::Real(v)) => SqlValue::Float(v),
            Ok(rusqlite::types::ValueRef::Text(bytes)) => {
                SqlValue::Text(String::from_utf8_lossy(bytes).into_owned())
            }
            Ok(rusqlite::types::ValueRef::Blob(bytes)) => SqlValue::Blob(bytes.to_vec()),
            Err(_) => SqlValue::Null,
        };
        columns.push(SqlColumn {
            name: col_names.get(i).cloned().unwrap_or_default(),
            value,
        });
    }
    SqlRow { columns }
}

#[cfg(test)]
thread_local! {
    static ROW_CONVERSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Bind `SqlValue` parameters to a rusqlite statement.
///
/// `pub(crate)` (ADR-099 B3 r6 structural cut): reused by the pure
/// `*_statement` builders in `stores::{entity,note,graph,text,vectors}` so
/// that every store's async execution path and the ADR-099 `--atomic`
/// prepare path bind params identically — one implementation, not two.
pub(crate) fn bind_params(
    stmt: &mut rusqlite::Statement<'_>,
    params: &[SqlValue],
) -> Result<(), rusqlite::Error> {
    for (i, param) in params.iter().enumerate() {
        let idx = i + 1; // rusqlite uses 1-based indexing
        match param {
            SqlValue::Null => stmt.raw_bind_parameter(idx, rusqlite::types::Null)?,
            SqlValue::Bool(v) => stmt.raw_bind_parameter(idx, *v as i64)?,
            SqlValue::Integer(v) => stmt.raw_bind_parameter(idx, *v)?,
            SqlValue::Float(v) => stmt.raw_bind_parameter(idx, *v)?,
            SqlValue::Text(v) => stmt.raw_bind_parameter(idx, v.as_str())?,
            SqlValue::Blob(v) => stmt.raw_bind_parameter(idx, v.as_slice())?,
            SqlValue::Json(v) => {
                let s = serde_json::to_string(v).unwrap_or_default();
                stmt.raw_bind_parameter(idx, s.as_str())?;
            }
            SqlValue::Uuid(v) => stmt.raw_bind_parameter(idx, v.to_string().as_str())?,
            SqlValue::Timestamp(v) => {
                stmt.raw_bind_parameter(idx, v.timestamp_micros())?;
            }
        }
    }
    Ok(())
}

/// Prepare exactly one [`SqlStatement`] SQL string.
///
/// rusqlite's `Connection::prepare` checks SQLite's returned tail and returns
/// `rusqlite::Error::MultipleStatement` when the tail contains another
/// executable statement (tail comments remain valid). Keeping this wrapper at
/// the bridge boundary makes the single-statement `SqlStatement` contract
/// explicit for queries and writes alike.
fn prepare_sql_statement<'conn>(
    conn: &'conn rusqlite::Connection,
    sql: &str,
) -> Result<rusqlite::Statement<'conn>, rusqlite::Error> {
    conn.prepare(sql)
}

/// Prepare one [`SqlStatement`] through rusqlite's per-connection LRU cache
/// while retaining the same single-statement tail validation as
/// [`prepare_sql_statement`].
fn prepare_cached_sql_statement<'conn>(
    conn: &'conn rusqlite::Connection,
    sql: &str,
) -> Result<rusqlite::CachedStatement<'conn>, rusqlite::Error> {
    conn.prepare_cached(sql)
}

/// A batch statement prepared once before execution. Ordinary SQL errors are
/// retried in the execution phase so they still exercise the owning
/// transaction's rollback path and can observe schema changes made by an
/// earlier statement in the same batch; only `MultipleStatement` aborts
/// preflight.
enum PreparedBatchStatement<'conn> {
    Ready(rusqlite::Statement<'conn>),
    PrepareAtExecution,
}

/// Prepare each batch statement exactly once while rejecting an executable
/// tail before any statement runs.
fn prepare_batch_statements<'conn>(
    conn: &'conn rusqlite::Connection,
    statements: &[SqlStatement],
) -> Result<Vec<PreparedBatchStatement<'conn>>, rusqlite::Error> {
    let mut prepared = Vec::with_capacity(statements.len());
    for statement in statements {
        match prepare_sql_statement(conn, &statement.sql) {
            Ok(statement) => prepared.push(PreparedBatchStatement::Ready(statement)),
            Err(error @ rusqlite::Error::MultipleStatement) => return Err(error),
            Err(error) => {
                crate::error::log_sqlite_full("execute_batch_preflight_deferred", &error);
                prepared.push(PreparedBatchStatement::PrepareAtExecution);
            }
        }
    }
    Ok(prepared)
}

/// Bind and execute handles returned by [`prepare_batch_statements`].
fn execute_prepared_batch<'conn>(
    conn: &'conn rusqlite::Connection,
    prepared: Vec<PreparedBatchStatement<'conn>>,
    statements: &[SqlStatement],
) -> Result<u64, rusqlite::Error> {
    debug_assert_eq!(prepared.len(), statements.len());
    let mut total = 0u64;
    for (prepared, statement) in prepared.into_iter().zip(statements) {
        let mut prepared = match prepared {
            PreparedBatchStatement::Ready(prepared) => prepared,
            PreparedBatchStatement::PrepareAtExecution => {
                prepare_sql_statement(conn, &statement.sql)?
            }
        };
        bind_params(&mut prepared, &statement.params)?;
        total += prepared.raw_execute()? as u64;
    }
    Ok(total)
}

/// SQL statement heads that are transaction control. `execute_batch` owns the
/// `BEGIN`/`COMMIT` boundary for the whole batch (the standalone path wraps
/// the list in its own `BEGIN IMMEDIATE`, and the queue-backed path runs
/// inside the writer task's per-request transaction), so a caller-supplied
/// statement that itself starts, ends, or branches a transaction can commit
/// or roll back early and break the batch's all-or-nothing contract. `START`
/// is classified as the alternate transaction-opening spelling so callers get
/// this typed boundary error; `END` is SQLite's `COMMIT` spelling.
const TRANSACTION_CONTROL_KEYWORDS: [&str; 7] = [
    "BEGIN",
    "START",
    "COMMIT",
    "END",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
];

/// Return the transaction-control keyword heading `sql`, if any.
///
/// Tolerates leading whitespace and `--` line / `/* */` block comments (SQLite
/// skips both before a statement) and matches the keyword case-insensitively
/// with a word-boundary check, so e.g. an identifier beginning with `begin`
/// never matches.
fn transaction_control_head(sql: &str) -> Option<&'static str> {
    let mut rest: &[u8] = sql.as_bytes();
    if rest.starts_with(b"\xEF\xBB\xBF") {
        rest = &rest[3..];
    }
    loop {
        let mut idx = 0;
        while idx < rest.len() && rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        rest = &rest[idx..];
        if let Some(tail) = rest.strip_prefix(b"--") {
            let mut idx = 0;
            while idx < tail.len() && tail[idx] != b'\n' {
                idx += 1;
            }
            rest = if idx < tail.len() {
                &tail[idx + 1..]
            } else {
                &[]
            };
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b"/*") {
            let mut idx = 0;
            while idx + 1 < tail.len() && !(tail[idx] == b'*' && tail[idx + 1] == b'/') {
                idx += 1;
            }
            rest = if idx + 1 < tail.len() {
                &tail[idx + 2..]
            } else {
                &[]
            };
            continue;
        }
        break;
    }
    TRANSACTION_CONTROL_KEYWORDS
        .iter()
        .copied()
        .find(|keyword| {
            let kw = keyword.as_bytes();
            if rest.len() < kw.len() || !rest[..kw.len()].eq_ignore_ascii_case(kw) {
                return false;
            }
            match rest.get(kw.len()) {
                Some(next) => !(next.is_ascii_alphanumeric() || *next == b'_'),
                None => true,
            }
        })
}

/// Reject transaction-control statements in `statements` with a typed
/// [`StorageError::InvalidInput`] BEFORE anything executes, preserving the
/// batch's all-or-nothing contract (see [`TRANSACTION_CONTROL_KEYWORDS`]).
fn reject_transaction_control_statements(
    statements: &[SqlStatement],
    operation: &'static str,
) -> khive_storage::types::StorageResult<()> {
    for (index, statement) in statements.iter().enumerate() {
        if let Some(keyword) = transaction_control_head(&statement.sql) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: operation.into(),
                message: format!(
                    "statement at index {index} is transaction control ({keyword}); \
                     execute_batch owns the BEGIN/COMMIT boundary for the whole \
                     batch — remove transaction-control statements from the batch"
                ),
            });
        }
    }
    Ok(())
}

/// One standalone-`execute_batch` failure, paired with the reason the handle
/// was poisoned (dropped instead of restored), if it was.
struct BatchFailure {
    error: rusqlite::Error,
    poison_reason: Option<BatchPoisonReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchHandleDisposition {
    Retain,
    Poison,
}

/// Execute one standalone batch while every prepared statement remains scoped
/// to the borrowed connection. The owned [`StandaloneHandle`] stays outside
/// this helper, so it can be restored or dropped only after all statement
/// borrows have ended.
fn execute_standalone_batch(
    conn: &rusqlite::Connection,
    statements: &[SqlStatement],
    origin: khive_storage::tx_registry::TxOrigin,
) -> (BatchHandleDisposition, Result<u64, BatchFailure>) {
    let prepared = match prepare_batch_statements(conn, statements) {
        Ok(prepared) => prepared,
        Err(error) => {
            return (
                BatchHandleDisposition::Retain,
                Err(BatchFailure {
                    error,
                    poison_reason: None,
                }),
            );
        }
    };
    if let Err(begin_error) = conn.execute_batch("BEGIN IMMEDIATE") {
        // Busy/locked is transient contention (another writer held SQLite's
        // write lock past `busy_timeout`); the connection itself is untouched,
        // so the handle remains reusable. Any other failure leaves transaction
        // state suspect and poisons the handle.
        drop(prepared);
        let (disposition, poison_reason) = if crate::timeout_sink::is_busy_or_locked(&begin_error) {
            (BatchHandleDisposition::Retain, None)
        } else {
            tracing::warn!(
                %begin_error,
                "execute_batch: BEGIN IMMEDIATE failed non-transiently; \
                 poisoning the standalone connection — the handle is \
                 dropped and must be re-acquired"
            );
            (
                BatchHandleDisposition::Poison,
                Some(BatchPoisonReason::BeginFailed),
            )
        };
        return (
            disposition,
            Err(BatchFailure {
                error: begin_error,
                poison_reason,
            }),
        );
    }

    // Registered only after BEGIN succeeds, and retained through COMMIT or
    // ROLLBACK so the registry never reports a transaction as finished early.
    let _tx_handle =
        khive_storage::tx_registry::register_scoped(Some("execute_batch".to_string()), origin);
    let result = (|| -> Result<u64, rusqlite::Error> {
        let total = execute_prepared_batch(conn, prepared, statements)?;
        conn.execute_batch("COMMIT")?;
        Ok(total)
    })();

    let mut disposition = BatchHandleDisposition::Retain;
    let mut poison_reason = None;
    if let Err(error) = &result {
        if let Err(rollback_error) = conn.execute_batch("ROLLBACK") {
            crate::error::log_sqlite_full("execute_batch_rollback", &rollback_error);
            // A failed ROLLBACK leaves the connection in an unknown
            // transaction state. Preserve the original statement error while
            // making the poison cause explicit to the caller.
            tracing::warn!(
                %error,
                %rollback_error,
                "execute_batch: ROLLBACK after statement failure failed; \
                 poisoning the standalone connection — the handle is \
                 dropped and must be re-acquired"
            );
            disposition = BatchHandleDisposition::Poison;
            poison_reason = Some(BatchPoisonReason::RollbackFailed(rollback_error));
        }
    }

    (
        disposition,
        result.map_err(|error| BatchFailure {
            error,
            poison_reason,
        }),
    )
}

#[derive(Debug)]
enum BatchPoisonReason {
    BeginFailed,
    RollbackFailed(rusqlite::Error),
}

impl std::fmt::Display for BatchPoisonReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeginFailed => f.write_str(
                "BEGIN IMMEDIATE failed non-transiently; connection transaction state is suspect",
            ),
            Self::RollbackFailed(error) => {
                write!(f, "ROLLBACK after statement failure failed: {error}")
            }
        }
    }
}

/// A `rusqlite::Error` whose display carries the poison context, so a
/// poisoned handle is visible to the caller in the returned error instead of
/// being discoverable only through later calls' generic "connection already
/// consumed" failures.
#[derive(Debug)]
struct PoisonedBatchError {
    original: rusqlite::Error,
    poison_reason: BatchPoisonReason,
}

impl std::fmt::Display for PoisonedBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; original error: {}",
            self.poison_reason, self.original
        )
    }
}

impl std::error::Error for PoisonedBatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.original)
    }
}

/// Execute a query on a `rusqlite::Connection` and return owned rows.
fn execute_query(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    let mut stmt = prepare_sql_statement(conn, &statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;

    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();

    let mut rows = Vec::new();
    let mut raw_rows = stmt.raw_query();
    while let Some(row) = raw_rows.next()? {
        rows.push(row_to_sql_row(row, col_count, &col_names));
    }
    Ok(rows)
}

fn execute_query_row(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Option<SqlRow>, rusqlite::Error> {
    let mut stmt = prepare_sql_statement(conn, &statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;

    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();

    let mut raw_rows = stmt.raw_query();
    Ok(raw_rows
        .next()?
        .map(|row| row_to_sql_row(row, col_count, &col_names)))
}

fn execute_query_page(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    page: &PageRequest,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    // A zero-limit page still prepares and binds the statement, so invalid
    // SQL fails identically across every limit; it skips the row cursor
    // entirely and returns no rows.
    if page.limit == 0 {
        let mut stmt = prepare_sql_statement(conn, &statement.sql)?;
        bind_params(&mut stmt, &statement.params)?;
        return Ok(Vec::new());
    }

    let mut stmt = prepare_sql_statement(conn, &statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;

    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();

    let mut rows = Vec::new();
    let mut offset = page.offset;
    let mut remaining = u64::from(page.limit);
    let mut raw_rows = stmt.raw_query();
    // The bound covers owned Rust rows only — this function advances past
    // `offset`, owns at most the caller-supplied `page.limit` rows, and drops
    // the statement cursor immediately afterward (ADR-005's bounded-
    // materialization amendment). Callers own choosing a sane limit.
    // Engine work is the query plan's own cost, not O(offset + limit):
    // SQLite still produces and discards `offset` rows, and an unindexed
    // ORDER BY can force a full sort of the result set before the first row
    // is stepped. Callers deep-paging a large result set should prefer
    // keyset pagination over growing offsets.
    while remaining > 0 {
        let Some(row) = raw_rows.next()? else {
            break;
        };
        if offset > 0 {
            offset -= 1;
            continue;
        }
        rows.push(row_to_sql_row(row, col_count, &col_names));
        remaining -= 1;
    }
    Ok(rows)
}

/// Map a rusqlite error to `StorageError`.
fn map_rusqlite_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    crate::error::storage_driver_error(StorageCapability::Sql, op, e)
}

async fn acquire_handle_slot(
    slots: Arc<Semaphore>,
    timeout: std::time::Duration,
    operation: &'static str,
) -> Result<OwnedSemaphorePermit, StorageError> {
    tokio::time::timeout(timeout, slots.acquire_owned())
        .await
        .map_err(|_| StorageError::Timeout {
            operation: operation.into(),
        })?
        .map_err(|error| StorageError::Pool {
            operation: operation.into(),
            message: error.to_string(),
        })
}

// =============================================================================
// Standalone connection readers/writers (file-backed databases)
// =============================================================================

fn open_standalone_reader(pool: &ConnectionPool) -> Result<rusqlite::Connection, StorageError> {
    let config = pool.config();
    let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
        operation: "reader".into(),
        message: "in-memory databases do not support standalone readers; use pool-backed".into(),
    })?;

    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| map_rusqlite_err(e, "open_reader"))?;

    conn.busy_timeout(config.busy_timeout)
        .map_err(|e| map_rusqlite_err(e, "open_reader"))?;
    conn.pragma_update(None, "cache_size", "-65536")
        .map_err(|e| map_rusqlite_err(e, "open_reader"))?;
    conn.pragma_update(None, "mmap_size", "1073741824")
        .map_err(|e| map_rusqlite_err(e, "open_reader"))?;

    Ok(conn)
}

fn open_standalone_writer(pool: &ConnectionPool) -> Result<rusqlite::Connection, StorageError> {
    let config = pool.config();
    let conn = pool
        .open_standalone_writer()
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "open_writer", e))?;

    conn.busy_timeout(config.busy_timeout)
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;
    conn.pragma_update(None, "cache_size", "-65536")
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;
    conn.pragma_update(None, "mmap_size", "1073741824")
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;

    Ok(conn)
}

/// Lift a standalone open onto the blocking thread pool while carrying the
/// connection-cap permit into the same closure.
///
/// If the awaiting future is cancelled, the detached blocking closure owns
/// both the connection result and the permit until it finishes, so the cap
/// cannot be released while an open is still running.
async fn open_standalone_on_blocking<F>(
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
async fn open_standalone_reader_on_blocking(
    pool: Arc<ConnectionPool>,
    slot: OwnedSemaphorePermit,
) -> khive_storage::types::StorageResult<(rusqlite::Connection, OwnedSemaphorePermit)> {
    open_standalone_on_blocking(pool, slot, "open_reader", open_standalone_reader).await
}

/// [`open_standalone_writer`] lifted onto the blocking thread pool; see
/// [`open_standalone_reader_on_blocking`] for the blocking rationale.
async fn open_standalone_writer_on_blocking(
    pool: Arc<ConnectionPool>,
    slot: OwnedSemaphorePermit,
) -> khive_storage::types::StorageResult<(rusqlite::Connection, OwnedSemaphorePermit)> {
    open_standalone_on_blocking(pool, slot, "open_writer", open_standalone_writer).await
}

// =============================================================================
// File-backed: SqliteReader (standalone connection)
// =============================================================================

struct StandaloneHandle {
    conn: rusqlite::Connection,
    /// Travels into every blocking closure with `conn`, so cancelling the
    /// awaiting task cannot release the cap while SQLite is still running.
    _slot: OwnedSemaphorePermit,
}

struct SqliteReader {
    handle: Option<StandaloneHandle>,
}

#[async_trait]
impl khive_storage::SqlReader for SqliteReader {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "query_row".into(),
            message: "connection already consumed".into(),
        })?;
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query_row(&handle.conn, &statement);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "query_row", e))?;
        self.handle = Some(handle);
        result.map_err(|e| map_rusqlite_err(e, "query_row"))
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "query_all".into(),
            message: "connection already consumed".into(),
        })?;
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&handle.conn, &statement);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "query_all", e))?;
        self.handle = Some(handle);
        result.map_err(|e| map_rusqlite_err(e, "query_all"))
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "query_page".into(),
            message: "connection already consumed".into(),
        })?;
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query_page(&handle.conn, &statement, &page);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "query_page", e))?;
        self.handle = Some(handle);
        result.map_err(|e| map_rusqlite_err(e, "query_page"))
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

// =============================================================================
// File-backed: SqliteWriter (standalone connection)
// =============================================================================

struct SqliteWriter {
    /// `None` at construction when a `WriterTaskHandle` was obtained (ADR-136
    /// D1 gate 1: queue-first `writer()` skips the standalone open in that
    /// case). Lazily opened by [`SqliteWriter::ensure_conn`] on first read
    /// (`query_row`/`query_all`/`query_page`) — production callers do read
    /// through a `writer()` handle (e.g. `khive-pack-comm` and
    /// `khive-pack-gtd`), so the `SqlReader` supertrait's lazy-open path is
    /// live, not just a capability formality. When present, the handle wraps
    /// the standalone connection together with its pool-wide permit — the
    /// one-permit writer budget for the eagerly opened read-write
    /// connection, a reader-permit for a lazily opened read-only one — and
    /// the whole handle is taken into every standalone blocking closure as
    /// one resource, so cancelling the awaiting task cannot release the cap
    /// while SQLite is still running; writer-task calls leave it resident
    /// because they use the task's connection.
    handle: Option<StandaloneHandle>,
    /// ADR-067 Component A: when the write queue is enabled, `execute_batch`
    /// routes the whole caller-supplied statement list through the
    /// single-writer task instead of opening its own `BEGIN IMMEDIATE` on
    /// the standalone connection. `None` when the flag is off or no writer
    /// task is available
    /// (best-effort — degrades to the standalone-connection path below).
    writer_task: Option<crate::writer_task::WriterTaskHandle>,
    /// The origin (ADR-091 backend-scoped attribution) of the pool this
    /// standalone connection was opened against.
    origin: khive_storage::tx_registry::TxOrigin,
    /// This connection's pool's writer-timeout sink identity (`db_label`),
    /// captured at construction so the standalone-path busy/locked mapping
    /// below doesn't need a `&ConnectionPool` reference to report against.
    db: String,
    /// Needed only for [`Self::ensure_conn`]'s lazy standalone-connection
    /// open when `handle` was skipped at construction (`writer_task`
    /// present).
    pool: Arc<ConnectionPool>,
}

impl SqliteWriter {
    fn check_write_capacity(&self, operation: &'static str) -> Result<(), StorageError> {
        self.pool
            .check_write_capacity()
            .map_err(|error| StorageError::driver(StorageCapability::Sql, operation, error))
    }

    /// Return the open handle if present, else lazily open a standalone
    /// **read-only** handle now. See the `handle` field doc comment for when
    /// this lazy path is reached — it is only reached from the `SqlReader`
    /// methods (`query_row`/`query_all`/`query_page`), never from a
    /// `SqlWriter` method: every `SqlWriter` method on this type either
    /// routes through `writer_task` (when present, the same condition that
    /// causes `handle` to start `None`) or uses the writer connection opened
    /// eagerly at construction (when `writer_task` is absent). Opening a
    /// read-only connection here (ADR-136 D1 gate 3 amendment) closes the
    /// gap where a caller holding a queue-backed writer handle could issue
    /// an `INSERT ... RETURNING` (or any other DML) through `query_row` /
    /// `query_all` / `query_page` and have it execute on an untracked
    /// read-write connection, outside the `WriterTask` — SQLite rejects DML
    /// against a read-only connection instead. The lazy open acquires a
    /// pool-wide READER permit rather than the one-permit writer budget:
    /// the connection cannot write, and charging it against the writer
    /// budget would let a queue-backed handle's reads block standalone
    /// writers. Like the eagerly opened writer connection's permit, the
    /// reader permit travels in the handle into every blocking closure.
    /// Associated function rather than a `&self` method so the caller can
    /// pass a cloned pool handle and keep no borrow of `SqliteWriter` live
    /// across the permit await — `&SqliteWriter` is not `Sync` (the held
    /// `rusqlite::Connection` is not), and the `SqlReader` futures must
    /// stay `Send`.
    async fn ensure_conn(
        pool: Arc<ConnectionPool>,
    ) -> khive_storage::types::StorageResult<StandaloneHandle> {
        let handle_slot = acquire_handle_slot(
            pool.sql_bridge_reader_slots(),
            pool.config().checkout_timeout,
            "sql_bridge.reader_handle",
        )
        .await?;
        let (conn, handle_slot) = open_standalone_reader_on_blocking(pool, handle_slot).await?;
        Ok(StandaloneHandle {
            conn,
            _slot: handle_slot,
        })
    }
}

#[async_trait]
impl khive_storage::SqlReader for SqliteWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None if self.writer_task.is_some() => Self::ensure_conn(Arc::clone(&self.pool)).await?,
            None => {
                return Err(StorageError::Pool {
                    operation: "writer.query_row".into(),
                    message: "connection already consumed".into(),
                })
            }
        };
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query_row(&handle.conn, &statement);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "writer.query_row", e))?;
        self.handle = Some(handle);
        result.map_err(|e| map_rusqlite_err(e, "writer.query_row"))
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None if self.writer_task.is_some() => Self::ensure_conn(Arc::clone(&self.pool)).await?,
            None => {
                return Err(StorageError::Pool {
                    operation: "writer.query_all".into(),
                    message: "connection already consumed".into(),
                })
            }
        };
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&handle.conn, &statement);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "writer.query_all", e))?;
        self.handle = Some(handle);
        result.map_err(|e| map_rusqlite_err(e, "writer.query_all"))
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None if self.writer_task.is_some() => Self::ensure_conn(Arc::clone(&self.pool)).await?,
            None => {
                return Err(StorageError::Pool {
                    operation: "writer.query_page".into(),
                    message: "connection already consumed".into(),
                })
            }
        };
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query_page(&handle.conn, &statement, &page);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "writer.query_page", e))?;
        self.handle = Some(handle);
        result.map_err(|e| map_rusqlite_err(e, "writer.query_page"))
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
        self.check_write_capacity("sql_bridge.execute.disk_reserve")?;
        // ADR-067 Component A (Fork C slice 2): a single statement is
        // self-contained, just like `execute_batch`'s full statement list —
        // transaction-control rejection remains an `execute_batch` contract;
        // this primitive is also used by internal atomic transaction owners.
        // route it through the writer task when available. `self.handle` is
        // left untouched so a subsequent `execute`/`execute_script` call on
        // this same handle still works over the standalone connection.
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send_bounded(move |conn| {
                    let mut stmt = prepare_cached_sql_statement(conn, &statement.sql)
                        .map_err(|e| map_rusqlite_err(e, "execute"))?;
                    bind_params(&mut stmt, &statement.params)
                        .map_err(|e| map_rusqlite_err(e, "execute"))?;
                    let affected = stmt
                        .raw_execute()
                        .map_err(|e| map_rusqlite_err(e, "execute"))?;
                    Ok(affected as u64)
                })
                .await;
        }

        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute".into(),
            message: "connection already consumed".into(),
        })?;
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = (|| -> Result<usize, rusqlite::Error> {
                let mut stmt = prepare_cached_sql_statement(&handle.conn, &statement.sql)?;
                bind_params(&mut stmt, &statement.params)?;
                stmt.raw_execute()
            })();
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute", e))?;
        self.handle = Some(handle);
        let affected = result.map_err(|e| {
            crate::timeout_sink::maybe_emit_busy(
                &self.db,
                crate::timeout_sink::Site::StandaloneSqlBridge,
                &e,
            );
            map_rusqlite_err(e, "execute")
        })?;
        Ok(affected as u64)
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::types::StorageResult<u64> {
        self.check_write_capacity("sql_bridge.execute_batch.disk_reserve")?;
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
            return writer_task
                .send_bounded(move |conn| {
                    let prepared = prepare_batch_statements(conn, &statements)
                        .map_err(|e| map_rusqlite_err(e, "execute_batch"))?;
                    execute_prepared_batch(conn, prepared, &statements)
                        .map_err(|e| map_rusqlite_err(e, "execute_batch"))
                })
                .await;
        }

        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_batch".into(),
            message: "connection already consumed".into(),
        })?;
        let origin = self.origin.clone();
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let (disposition, result) = execute_standalone_batch(&handle.conn, &statements, origin);
            let retained = match disposition {
                BatchHandleDisposition::Retain => Some(handle),
                BatchHandleDisposition::Poison => None,
            };
            (retained, result)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_batch", e))?;
        self.handle = handle;
        result.map_err(|failure| {
            crate::error::log_sqlite_full("execute_batch", &failure.error);
            crate::timeout_sink::maybe_emit_busy(
                &self.db,
                crate::timeout_sink::Site::StandaloneSqlBridge,
                &failure.error,
            );
            match failure.poison_reason {
                Some(poison_reason) => StorageError::driver(
                    StorageCapability::Sql,
                    "execute_batch",
                    PoisonedBatchError {
                        original: failure.error,
                        poison_reason,
                    },
                ),
                None => map_rusqlite_err(failure.error, "execute_batch"),
            }
        })
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        self.check_write_capacity("sql_bridge.execute_script.disk_reserve")?;
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
                .send_bounded(move |conn| {
                    conn.execute_batch(&script)
                        .map_err(|e| map_rusqlite_err(e, "execute_script"))
                })
                .await;
        }

        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script".into(),
            message: "connection already consumed".into(),
        })?;
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = handle.conn.execute_batch(&script);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script", e))?;
        self.handle = Some(handle);
        result.map_err(|e| {
            crate::timeout_sink::maybe_emit_busy(
                &self.db,
                crate::timeout_sink::Site::StandaloneSqlBridge,
                &e,
            );
            map_rusqlite_err(e, "execute_script")
        })
    }

    async fn execute_script_top_level(
        &mut self,
        script: String,
    ) -> khive_storage::types::StorageResult<()> {
        self.check_write_capacity("sql_bridge.execute_script_top_level.disk_reserve")?;
        // Boundary: this internal maintenance/migration path deliberately
        // bypasses the `execute_batch` transaction-control rejection.
        // ADR-067 Component A: unlike
        // `execute_script`, this must NOT run inside the writer task's
        // per-request `BEGIN IMMEDIATE` — statements such as VACUUM are
        // rejected by SQLite inside any open transaction. Route through
        // `WriterTaskHandle::send_top_level`, which still serializes this
        // call through the single writer owner but skips the transaction
        // wrap entirely.
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send_top_level_bounded(move |conn| {
                    conn.execute_batch(&script)
                        .map_err(|e| map_rusqlite_err(e, "execute_script_top_level"))
                })
                .await;
        }

        // Flag off / no writer task: identical to `execute_script`'s own
        // flag-off path — a bare `execute_batch` on the standalone
        // connection, already transaction-free.
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script_top_level".into(),
            message: "connection already consumed".into(),
        })?;
        let (handle, result) = tokio::task::spawn_blocking(move || {
            let res = handle.conn.execute_batch(&script);
            (handle, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script_top_level", e))?;
        self.handle = Some(handle);
        result.map_err(|e| {
            crate::timeout_sink::maybe_emit_busy(
                &self.db,
                crate::timeout_sink::Site::StandaloneSqlBridge,
                &e,
            );
            map_rusqlite_err(e, "execute_script_top_level")
        })
    }
}

// =============================================================================
// Pool-backed reader/writer (in-memory databases)
// =============================================================================

struct PoolBackedReader {
    pool: Arc<ConnectionPool>,
}

#[async_trait]
impl khive_storage::SqlReader for PoolBackedReader {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool
                .reader()
                .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_reader", e))?;
            execute_query_row(&guard, &statement)
                .map_err(|e| map_rusqlite_err(e, "pool_reader.query_row"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_reader.query_row", e))?
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool
                .reader()
                .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_reader", e))?;
            execute_query(&guard, &statement)
                .map_err(|e| map_rusqlite_err(e, "pool_reader.query_all"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_reader.query_all", e))?
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool
                .reader()
                .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_reader", e))?;
            execute_query_page(&guard, &statement, &page)
                .map_err(|e| map_rusqlite_err(e, "pool_reader.query_page"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_reader.query_page", e))?
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

struct PoolBackedWriter {
    pool: Arc<ConnectionPool>,
}

#[async_trait]
impl khive_storage::SqlReader for PoolBackedWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.query_row", e)
            })?;
            execute_query_row(&guard, &statement)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.query_row"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_writer.query_row", e))?
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.query_all", e)
            })?;
            execute_query(&guard, &statement)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.query_all"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_writer.query_all", e))?
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.query_page", e)
            })?;
            execute_query_page(&guard, &statement, &page)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.query_page"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_writer.query_page", e))?
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
        // Boundary: `execute_batch` owns transaction-control rejection;
        // this one-statement primitive is used by internal DML/transaction
        // owners and is still guarded by the SqlStatement single-statement
        // prepare contract.
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute", e)
            })?;
            let mut stmt = prepare_cached_sql_statement(&guard, &statement.sql)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute"))?;
            bind_params(&mut stmt, &statement.params)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute"))?;
            let rows = stmt
                .raw_execute()
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute"))?;
            Ok(rows as u64)
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
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_batch", e)
            })?;
            let prepared = prepare_batch_statements(&guard, &statements)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
            guard
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some("pool_writer.execute_batch".to_string()),
                pool.origin(),
            );
            let result = execute_prepared_batch(&guard, prepared, &statements)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"));
            match result {
                Ok(total) => {
                    if let Err(e) = guard.execute_batch("COMMIT") {
                        crate::error::log_ignored_sqlite_result(
                            "pool_writer_execute_batch_commit_rollback",
                            guard.execute_batch("ROLLBACK"),
                        );
                        Err(map_rusqlite_err(e, "pool_writer.execute_batch"))
                    } else {
                        Ok(total)
                    }
                }
                Err(e) => {
                    crate::error::log_ignored_sqlite_result(
                        "pool_writer_execute_batch_rollback",
                        guard.execute_batch("ROLLBACK"),
                    );
                    Err(e)
                }
            }
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "pool_writer.execute_batch", e))?
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        // Boundary: raw scripts are internal/migration-only and do not inherit
        // `execute_batch`'s transaction-control rejection.
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_script", e)
            })?;
            guard
                .execute_batch(&script)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_script"))
        })
        .await
        .map_err(|e| {
            StorageError::driver(StorageCapability::Sql, "pool_writer.execute_script", e)
        })?
    }
}

// =============================================================================
// atomic_unit (ADR-067 Component A, Fork C slice 2)
// =============================================================================

/// A purely-synchronous `SqlReader`/`SqlWriter` over a borrowed connection,
/// used ONLY to drive an [`AtomicUnitOp`] on the flag-on path, where the
/// closure body runs inside the writer task's `spawn_blocking` (synchronous
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
/// every dereference) is upheld by construction — see `atomic_unit`, the
/// only call site: it builds an `InlineWriter` from `conn: &Connection`,
/// drives `op` to completion via `block_on_sync` synchronously, and drops
/// the `InlineWriter` before that borrow ends, all within one stack frame.
struct InlineWriter {
    conn: *const rusqlite::Connection,
}

// SAFETY: `InlineWriter` is never actually shared across a real thread
// boundary — it is constructed, driven to completion synchronously via
// `block_on_sync`, and dropped within a single call frame inside the
// writer task's `spawn_blocking` closure (see `atomic_unit`). The `Send`
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
}

#[async_trait]
impl khive_storage::SqlReader for InlineWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        execute_query_row(self.conn(), &statement)
            .map_err(|e| map_rusqlite_err(e, "inline.query_row"))
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        execute_query(self.conn(), &statement).map_err(|e| map_rusqlite_err(e, "inline.query_all"))
    }

    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        execute_query_page(self.conn(), &statement, &page)
            .map_err(|e| map_rusqlite_err(e, "inline.query_page"))
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
        let mut stmt = prepare_cached_sql_statement(self.conn(), &statement.sql)
            .map_err(|e| map_rusqlite_err(e, "inline.execute"))?;
        bind_params(&mut stmt, &statement.params)
            .map_err(|e| map_rusqlite_err(e, "inline.execute"))?;
        let affected = stmt
            .raw_execute()
            .map_err(|e| map_rusqlite_err(e, "inline.execute"))?;
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
        reject_transaction_control_statements(&statements, "inline.execute_batch")?;
        let prepared = prepare_batch_statements(self.conn(), &statements)
            .map_err(|e| map_rusqlite_err(e, "inline.execute_batch"))?;
        execute_prepared_batch(self.conn(), prepared, &statements)
            .map_err(|e| map_rusqlite_err(e, "inline.execute_batch"))
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        // Boundary: this raw script path is internal maintenance only and is
        // outside the `execute_batch` transaction-control contract.
        self.conn()
            .execute_batch(&script)
            .map_err(|e| map_rusqlite_err(e, "inline.execute_script"))
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
fn block_on_sync<F: std::future::Future>(fut: F) -> Result<F::Output, StorageError> {
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
            tracing::error!(
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

/// Run `op` under a manual `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` on `writer`
/// — the pre-ADR-067 shape, used by [`SqlBridge::atomic_unit`] whenever no
/// writer task applies (flag off, no runtime, or an in-memory pool),
/// preserving that path byte-for-byte.
async fn run_manual_atomic_unit(
    writer: &mut dyn khive_storage::SqlWriter,
    op: AtomicUnitOp,
    origin: khive_storage::tx_registry::TxOrigin,
) -> khive_storage::types::StorageResult<Box<dyn Any + Send>> {
    fn tx_stmt(sql: &str, label: &str) -> SqlStatement {
        SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: Some(label.to_string()),
        }
    }
    khive_storage::SqlWriter::execute(writer, tx_stmt("BEGIN IMMEDIATE", "begin")).await?;
    let _tx_handle =
        khive_storage::tx_registry::register_scoped(Some("atomic_unit".to_string()), origin);

    let result = op(writer).await;

    match result {
        Ok(value) => {
            match khive_storage::SqlWriter::execute(writer, tx_stmt("COMMIT", "commit")).await {
                Ok(_) => Ok(value),
                Err(e) => {
                    let _ =
                        khive_storage::SqlWriter::execute(writer, tx_stmt("ROLLBACK", "rollback"))
                            .await;
                    Err(e)
                }
            }
        }
        Err(e) => {
            let _ =
                khive_storage::SqlWriter::execute(writer, tx_stmt("ROLLBACK", "rollback")).await;
            Err(e)
        }
    }
}

// =============================================================================
// SqlBridge: the SqlAccess implementor
// =============================================================================

/// Bridges `ConnectionPool` to `khive_storage::SqlAccess`.
///
/// Dispatches based on whether the pool is file-backed or in-memory:
/// - File-backed: standalone connections per reader/writer handle, capped per
///   pool at the effective reader count and one writer; atomic units drive a
///   single registered raw transaction span instead of a caller-held per-tx
///   connection.
/// - In-memory: pool-backed connections per query (single shared connection).
pub struct SqlBridge {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
}

impl SqlBridge {
    /// Create a new bridge wrapping the given pool.
    pub fn new(pool: Arc<ConnectionPool>, is_file_backed: bool) -> Self {
        Self {
            pool,
            is_file_backed,
        }
    }
}

#[async_trait]
impl khive_storage::SqlAccess for SqlBridge {
    async fn reader(
        &self,
    ) -> khive_storage::types::StorageResult<Box<dyn khive_storage::SqlReader>> {
        if self.is_file_backed {
            let handle_slot = acquire_handle_slot(
                self.pool.sql_bridge_reader_slots(),
                self.pool.config().checkout_timeout,
                "sql_bridge.reader_handle",
            )
            .await?;
            let (conn, handle_slot) =
                open_standalone_reader_on_blocking(Arc::clone(&self.pool), handle_slot).await?;
            Ok(Box::new(SqliteReader {
                handle: Some(StandaloneHandle {
                    conn,
                    _slot: handle_slot,
                }),
            }))
        } else {
            Ok(Box::new(PoolBackedReader {
                pool: Arc::clone(&self.pool),
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
                    tracing::warn!(
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
            // lazily open a read-only one on first use in the handle-present
            // case; production callers do read through a `writer()` handle,
            // so this lazy path is live (see `SqliteWriter::ensure_conn`).
            // The standalone open acquires the pool-wide one-permit writer
            // budget first, and the permit travels in the handle for the
            // handle's whole lifetime — a queue-backed handle holds no
            // writer permit (its writes route through the writer task), so
            // this budget caps exactly the standalone read-write
            // connections.
            let handle = if writer_task.is_none() {
                let handle_slot = acquire_handle_slot(
                    self.pool.sql_bridge_writer_slots(),
                    self.pool.config().checkout_timeout,
                    "sql_bridge.writer_handle",
                )
                .await?;
                let (conn, handle_slot) =
                    open_standalone_writer_on_blocking(Arc::clone(&self.pool), handle_slot).await?;
                Some(StandaloneHandle {
                    conn,
                    _slot: handle_slot,
                })
            } else {
                None
            };
            Ok(Box::new(SqliteWriter {
                handle,
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
    /// (`SqlAccess::atomic_unit`'s doc comment): on the flag-on branch below,
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
        if self.is_file_backed {
            if self.pool.config().read_only {
                return Err(StorageError::Pool {
                    operation: "atomic_unit".into(),
                    message: "backend is read-only".into(),
                });
            }
            self.pool.check_write_capacity().map_err(|error| {
                StorageError::driver(StorageCapability::Sql, "atomic_unit.disk_reserve", error)
            })?;
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
                return writer_task
                    .send_bounded(move |conn| {
                        let mut inline = InlineWriter {
                            conn: conn as *const rusqlite::Connection,
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
                    })
                    .await;
            }
            // Flag-off (or no writer task available): manual
            // BEGIN IMMEDIATE/COMMIT/ROLLBACK on a standalone writer —
            // byte-for-byte the pre-ADR-067 shape.
            //
            // Contract: this acquire waits on the pool-wide one-permit
            // writer-handle budget — the same permit a live `writer()` handle
            // holds for its lifetime — so it times out with
            // `StorageError::Timeout` after `checkout_timeout` while a writer
            // handle is checked out (and a `writer()` call times out while
            // this unit runs). Callers must not hold a boxed writer handle
            // across an `atomic_unit()` call on the same pool; drop the
            // handle first. The `writer_task` branch above never touches this
            // budget.
            let handle_slot = acquire_handle_slot(
                self.pool.sql_bridge_writer_slots(),
                self.pool.config().checkout_timeout,
                "sql_bridge.atomic_unit_handle",
            )
            .await?;
            let (conn, handle_slot) =
                open_standalone_writer_on_blocking(Arc::clone(&self.pool), handle_slot).await?;
            let mut writer = SqliteWriter {
                handle: Some(StandaloneHandle {
                    conn,
                    _slot: handle_slot,
                }),
                writer_task: None,
                origin: self.pool.origin(),
                db: crate::timeout_sink::db_label(&self.pool),
                pool: Arc::clone(&self.pool),
            };
            run_manual_atomic_unit(&mut writer, op, self.pool.origin()).await
        } else {
            // In-memory pools are exempt (not accept-loop reachable, per the
            // rework spec's "Out of scope") — preserve the existing
            // pool-backed manual-transaction behavior.
            let mut writer = PoolBackedWriter {
                pool: Arc::clone(&self.pool),
            };
            run_manual_atomic_unit(&mut writer, op, self.pool.origin()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;
    use khive_storage::types::{SqlStatement, SqlValue};
    use khive_storage::{SqlAccess as _, SqlReader as _};

    struct NotifyOnDrop(Arc<tokio::sync::Notify>);

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    fn blocking_progress_gate(
        conn: &rusqlite::Connection,
    ) -> (
        Arc<tokio::sync::Notify>,
        Arc<std::sync::Barrier>,
        Arc<tokio::sync::Notify>,
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let callback_entered = Arc::clone(&entered);
        let release = Arc::new(std::sync::Barrier::new(2));
        let callback_release = Arc::clone(&release);
        let completed = Arc::new(tokio::sync::Notify::new());
        let notify_on_drop = NotifyOnDrop(Arc::clone(&completed));
        let blocked_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_blocked_once = Arc::clone(&blocked_once);
        conn.progress_handler(
            1,
            Some(move || {
                let _keep_until_connection_drop = &notify_on_drop;
                if !callback_blocked_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    callback_entered.notify_one();
                    callback_release.wait();
                    return true;
                }
                false
            }),
        )
        .unwrap();
        (entered, release, completed)
    }

    fn progress_gate_statement() -> SqlStatement {
        SqlStatement {
            sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 999\
                  ) SELECT SUM(value) FROM rows"
                .into(),
            params: vec![],
            label: None,
        }
    }

    #[test]
    fn query_row_converts_only_the_first_matching_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let statement = SqlStatement {
            sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 99\
                  ) SELECT value FROM rows ORDER BY value"
                .into(),
            params: vec![],
            label: None,
        };

        ROW_CONVERSIONS.with(|count| count.set(0));
        let row = execute_query_row(&conn, &statement).unwrap().unwrap();

        assert!(matches!(row.get("value"), Some(SqlValue::Integer(0))));
        ROW_CONVERSIONS.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn query_page_bounds_owned_rows_before_full_materialization() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let statement = SqlStatement {
            sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 99\
                  ) SELECT value FROM rows ORDER BY value"
                .into(),
            params: vec![],
            label: None,
        };

        ROW_CONVERSIONS.with(|count| count.set(0));
        let rows = execute_query_page(
            &conn,
            &statement,
            &PageRequest {
                offset: 40,
                limit: 3,
            },
        )
        .unwrap();

        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].get("value"), Some(SqlValue::Integer(40))));
        assert!(matches!(rows[2].get("value"), Some(SqlValue::Integer(42))));
        ROW_CONVERSIONS.with(|count| assert_eq!(count.get(), 3));
    }

    #[test]
    fn query_page_zero_limit_converts_no_rows_but_still_validates_sql() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let statement = SqlStatement {
            sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 99\
                  ) SELECT value FROM rows ORDER BY value"
                .into(),
            params: vec![],
            label: None,
        };

        ROW_CONVERSIONS.with(|count| count.set(0));
        let rows = execute_query_page(
            &conn,
            &statement,
            &PageRequest {
                offset: 0,
                limit: 0,
            },
        )
        .unwrap();

        assert!(rows.is_empty());
        ROW_CONVERSIONS.with(|count| assert_eq!(count.get(), 0));

        let invalid = SqlStatement {
            sql: "SELECT FROM WHERE".into(),
            params: vec![],
            label: None,
        };
        assert!(
            execute_query_page(
                &conn,
                &invalid,
                &PageRequest {
                    offset: 0,
                    limit: 0
                }
            )
            .is_err(),
            "a zero-limit page must still fail on invalid SQL at prepare time"
        );
    }

    #[test]
    fn cached_writer_prepare_preserves_the_single_statement_boundary() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        assert!(matches!(
            prepare_cached_sql_statement(&conn, "SELECT 1; SELECT 2"),
            Err(rusqlite::Error::MultipleStatement)
        ));
    }

    #[tokio::test]
    async fn queue_backed_execute_reuses_the_persistent_connection_statement_cache() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("sql_bridge_writer_cache.db")),
                write_queue_enabled: Some(true),
                write_routing_strict: true,
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE writer_cache_test (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
            )
            .unwrap();

        let writer_task = pool
            .writer_task_handle()
            .unwrap()
            .expect("file-backed queue-enabled pool must expose its writer task");
        let prepare_count = Arc::new(AtomicUsize::new(0));
        let hook_count = Arc::clone(&prepare_count);
        writer_task
            .send_top_level(move |conn| {
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Insert { table_name } if table_name == "writer_cache_test"
                    ) {
                        hook_count.fetch_add(1, Ordering::SeqCst);
                    }
                    Authorization::Allow
                }))
                .map_err(|error| map_rusqlite_err(error, "test.install_authorizer"))
            })
            .await
            .unwrap();

        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        for id in [1, 2] {
            khive_storage::SqlWriter::execute(
                &mut *writer,
                SqlStatement {
                    sql: "INSERT INTO writer_cache_test (id, value) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(id), SqlValue::Text(format!("value-{id}"))],
                    label: None,
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(
            prepare_count.load(Ordering::SeqCst),
            1,
            "the second identical execute on the writer task's persistent connection must reuse \
             the cached SQLite statement instead of compiling it again"
        );
        writer_task
            .send_top_level(|conn| {
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .map_err(|error| map_rusqlite_err(error, "test.remove_authorizer"))
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retained_standalone_writer_rechecks_disk_reserve_on_every_write_call() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("retained-writer-disk-reserve.db")),
                write_queue_enabled: Some(false),
                disk_reserve_bytes: 1,
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();
        khive_storage::SqlWriter::execute_script(
            &mut *writer,
            "CREATE TABLE capacity_call (id INTEGER PRIMARY KEY)".to_string(),
        )
        .await
        .unwrap();

        // The handle and its RW connection predate this capacity change. A
        // connection-open-only guard would incorrectly let the INSERT run.
        pool.force_available_bytes_for_test(0);
        let error = khive_storage::SqlWriter::execute(
            &mut *writer,
            SqlStatement {
                sql: "INSERT INTO capacity_call (id) VALUES (1)".to_string(),
                params: vec![],
                label: Some("capacity-refused".to_string()),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            StorageError::Driver { ref source, .. }
                if matches!(
                    source.downcast_ref::<SqliteError>(),
                    Some(SqliteError::DiskCapacityFloor {
                        available_bytes: 0,
                        reserve_bytes: 1,
                        ..
                    })
                )
        ));

        pool.force_available_bytes_for_test(2);
        khive_storage::SqlWriter::execute(
            &mut *writer,
            SqlStatement {
                sql: "INSERT INTO capacity_call (id) VALUES (1)".to_string(),
                params: vec![],
                label: Some("capacity-recovered".to_string()),
            },
        )
        .await
        .expect("capacity recovery must not poison a retained writer handle");
    }

    #[test]
    fn inline_execute_batch_prepares_each_statement_once() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE single_prepare_test (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .unwrap();
        let prepare_count = Arc::new(AtomicUsize::new(0));
        let hook_count = Arc::clone(&prepare_count);
        conn.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Insert { table_name } if table_name == "single_prepare_test"
            ) {
                hook_count.fetch_add(1, Ordering::SeqCst);
            }
            Authorization::Allow
        }))
        .unwrap();

        let mut writer = InlineWriter {
            conn: &conn as *const rusqlite::Connection,
        };
        let affected = block_on_sync(khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![SqlStatement {
                sql: "INSERT INTO single_prepare_test (id, value) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(1), SqlValue::Text("once".into())],
                label: None,
            }],
        ))
        .expect("InlineWriter operations must resolve on their first poll")
        .expect("valid batch must execute");

        assert_eq!(affected, 1);
        assert_eq!(
            prepare_count.load(Ordering::SeqCst),
            1,
            "classification and execution must share one prepared statement handle"
        );
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
    }

    #[test]
    fn inline_execute_batch_preserves_schema_dependencies_between_statements() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let mut writer = InlineWriter {
            conn: &conn as *const rusqlite::Connection,
        };

        let affected = block_on_sync(khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![
                SqlStatement {
                    sql: "CREATE TABLE dependent_prepare_test (id INTEGER PRIMARY KEY)".into(),
                    params: vec![],
                    label: None,
                },
                SqlStatement {
                    sql: "INSERT INTO dependent_prepare_test (id) VALUES (1)".into(),
                    params: vec![],
                    label: None,
                },
            ],
        ))
        .expect("InlineWriter operations must resolve on their first poll")
        .expect("a later statement must be prepared after its prerequisite schema change");

        assert_eq!(affected, 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM dependent_prepare_test", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn pool_backed_query_page_beyond_result_set_returns_empty() {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let writer = pool.writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE page_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL);\
                     INSERT INTO page_test (id, val) VALUES (1, 'a'), (2, 'b'), (3, 'c');",
                )
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), false);

        let statement = || SqlStatement {
            sql: "SELECT val FROM page_test ORDER BY id".into(),
            params: vec![],
            label: None,
        };

        let mut reader = bridge.reader().await.unwrap();
        let page = reader
            .query_page(
                statement(),
                PageRequest {
                    offset: 1,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
        assert!(matches!(page[0].get("val"), Some(SqlValue::Text(v)) if v == "b"));
        assert!(matches!(page[1].get("val"), Some(SqlValue::Text(v)) if v == "c"));

        let empty = reader
            .query_page(
                statement(),
                PageRequest {
                    offset: 99,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(
            empty.is_empty(),
            "offset past the last row must return an empty page, got {empty:?}"
        );
        drop(reader);

        let mut writer = bridge.writer().await.unwrap();
        let empty = writer
            .query_page(
                statement(),
                PageRequest {
                    offset: 99,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(
            empty.is_empty(),
            "offset past the last row must return an empty page, got {empty:?}"
        );
    }

    #[tokio::test]
    async fn file_bridge_caps_live_reader_and_writer_handles_per_pool() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_handle_cap.db")),
            write_queue_enabled: Some(false),
            max_readers: 2,
            checkout_timeout: std::time::Duration::from_millis(20),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let second_bridge = SqlBridge::new(Arc::clone(&pool), true);

        let reader_a = bridge.reader().await.unwrap();
        let reader_b = second_bridge.reader().await.unwrap();
        let reader_error = match second_bridge.reader().await {
            Ok(_) => panic!("a third live reader handle exceeded the configured cap"),
            Err(error) => error,
        };
        assert!(matches!(
            reader_error,
            StorageError::Timeout { ref operation }
                if operation.as_ref() == "sql_bridge.reader_handle"
        ));
        drop((reader_a, reader_b));
        let mut reader_after_release = bridge.reader().await.unwrap();
        let page = reader_after_release
            .query_page(
                SqlStatement {
                    sql: "WITH RECURSIVE rows(value) AS (\
                          SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 9\
                          ) SELECT value FROM rows ORDER BY value"
                        .into(),
                    params: vec![],
                    label: None,
                },
                PageRequest {
                    offset: 7,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
        assert!(matches!(page[0].get("value"), Some(SqlValue::Integer(7))));
        assert!(matches!(page[1].get("value"), Some(SqlValue::Integer(8))));
        drop(reader_after_release);

        let writer = bridge.writer().await.unwrap();
        let writer_error = match second_bridge.writer().await {
            Ok(_) => panic!("a second live writer handle exceeded the one-handle cap"),
            Err(error) => error,
        };
        assert!(matches!(
            writer_error,
            StorageError::Timeout { ref operation }
                if operation.as_ref() == "sql_bridge.writer_handle"
        ));
        drop(writer);
        let writer_after_release = bridge.writer().await.unwrap();
        drop(writer_after_release);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_standalone_open_retains_slot_until_open_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_open.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let slots = pool.sql_bridge_reader_slots();
        let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
        assert_eq!(slots.available_permits(), 0);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let open = tokio::spawn(open_standalone_on_blocking(
            Arc::clone(&pool),
            slot,
            "test_open_reader",
            move |pool| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                open_standalone_reader(pool)
            },
        ));
        tokio::task::spawn_blocking(move || entered_rx.recv())
            .await
            .unwrap()
            .unwrap();

        open.abort();
        assert!(matches!(open.await, Err(error) if error.is_cancelled()));
        assert_eq!(
            slots.available_permits(),
            0,
            "the permit must remain in the detached open closure"
        );
        let contender = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            Arc::clone(&slots).acquire_owned(),
        )
        .await;
        assert!(contender.is_err(), "an in-flight open must retain the cap");

        release_tx.send(()).unwrap();
        let recovered = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            Arc::clone(&slots).acquire_owned(),
        )
        .await
        .expect("the detached open did not release its permit")
        .unwrap();
        assert_eq!(slots.available_permits(), 0);
        drop(recovered);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_reader_query_retains_slot_until_blocking_work_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_reader.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let handle_slot = pool
            .sql_bridge_reader_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_reader(&pool).unwrap();
        let (entered, release, completed) = blocking_progress_gate(&conn);
        let mut reader = SqliteReader {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
        };
        let query = tokio::spawn(async move { reader.query_all(progress_gate_statement()).await });

        entered.notified().await;
        query.abort();
        let cancelled = matches!(query.await, Err(error) if error.is_cancelled());

        let contender = bridge.reader().await;
        let retained_slot = matches!(
            &contender,
            Err(StorageError::Timeout { operation })
                if operation.as_ref() == "sql_bridge.reader_handle"
        );
        drop(contender);

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("cancelled reader's detached SQLite call did not finish");
        assert!(cancelled, "reader query task did not report cancellation");
        assert!(
            retained_slot,
            "cancellation released the reader slot before SQLite stopped"
        );
        let reader_after_completion = bridge.reader().await.unwrap();
        drop(reader_after_completion);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_writer_query_retains_slot_until_blocking_work_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_writer.db")),
            write_queue_enabled: Some(false),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let handle_slot = pool
            .sql_bridge_writer_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, completed) = blocking_progress_gate(&conn);
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        let query = tokio::spawn(async move {
            khive_storage::SqlReader::query_all(&mut writer, progress_gate_statement()).await
        });

        entered.notified().await;
        query.abort();
        let cancelled = matches!(query.await, Err(error) if error.is_cancelled());

        let contender = bridge.writer().await;
        let retained_slot = matches!(
            &contender,
            Err(StorageError::Timeout { operation })
                if operation.as_ref() == "sql_bridge.writer_handle"
        );
        drop(contender);

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("cancelled writer's detached SQLite call did not finish");
        assert!(cancelled, "writer query task did not report cancellation");
        assert!(
            retained_slot,
            "cancellation released the writer slot before SQLite stopped"
        );
        let writer_after_completion = bridge.writer().await.unwrap();
        drop(writer_after_completion);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_reader_query_page_retains_slot_until_blocking_work_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_reader_page.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let handle_slot = pool
            .sql_bridge_reader_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_reader(&pool).unwrap();
        let (entered, release, completed) = blocking_progress_gate(&conn);
        let mut reader = SqliteReader {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
        };
        let query = tokio::spawn(async move {
            reader
                .query_page(
                    progress_gate_statement(),
                    PageRequest {
                        offset: 0,
                        limit: 10,
                    },
                )
                .await
        });

        entered.notified().await;
        query.abort();
        let cancelled = matches!(query.await, Err(error) if error.is_cancelled());

        let contender = bridge.reader().await;
        let retained_slot = matches!(
            &contender,
            Err(StorageError::Timeout { operation })
                if operation.as_ref() == "sql_bridge.reader_handle"
        );
        drop(contender);

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("cancelled reader's detached SQLite call did not finish");
        assert!(cancelled, "reader query task did not report cancellation");
        assert!(
            retained_slot,
            "cancellation released the reader slot before SQLite stopped"
        );
        let reader_after_completion = bridge.reader().await.unwrap();
        drop(reader_after_completion);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_writer_execute_batch_retains_slot_until_blocking_work_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_writer_batch.db")),
            write_queue_enabled: Some(false),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let handle_slot = pool
            .sql_bridge_writer_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, completed) = blocking_progress_gate(&conn);
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        let query = tokio::spawn(async move {
            khive_storage::SqlWriter::execute_batch(&mut writer, vec![progress_gate_statement()])
                .await
        });

        entered.notified().await;
        query.abort();
        let cancelled = matches!(query.await, Err(error) if error.is_cancelled());

        let contender = bridge.writer().await;
        let retained_slot = matches!(
            &contender,
            Err(StorageError::Timeout { operation })
                if operation.as_ref() == "sql_bridge.writer_handle"
        );
        drop(contender);

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("cancelled writer's detached SQLite call did not finish");
        assert!(cancelled, "writer batch task did not report cancellation");
        assert!(
            retained_slot,
            "cancellation released the writer slot before SQLite stopped"
        );
        let writer_after_completion = bridge.writer().await.unwrap();
        drop(writer_after_completion);
    }

    /// Cancelling an in-flight call permanently invalidates the boxed handle:
    /// the call took the handle's connection into the detached blocking task,
    /// so every subsequent call on the SAME handle fails loudly with
    /// "connection already consumed" instead of silently operating on a
    /// connection that may still be running the cancelled statement.
    /// Callers that cancel or time out a bridge call must drop the handle and
    /// acquire a fresh one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_call_invalidates_handle_reuse_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_reuse.db")),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());

        let handle_slot = acquire_handle_slot(
            pool.sql_bridge_writer_slots(),
            pool.config().checkout_timeout,
            "sql_bridge.writer_handle",
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, completed) = blocking_progress_gate(&conn);
        let writer = Arc::new(tokio::sync::Mutex::new(SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        }));
        let writer_clone = Arc::clone(&writer);
        let query = tokio::spawn(async move {
            khive_storage::SqlWriter::execute_batch(
                &mut *writer_clone.lock().await,
                vec![progress_gate_statement()],
            )
            .await
        });

        entered.notified().await;
        query.abort();
        let cancelled = matches!(query.await, Err(error) if error.is_cancelled());

        let reuse = khive_storage::SqlWriter::execute(
            &mut *writer.lock().await,
            SqlStatement {
                sql: "CREATE TABLE cancelled_reuse_probe (id INTEGER PRIMARY KEY)".into(),
                params: vec![],
                label: None,
            },
        )
        .await;
        let message = match reuse {
            Err(StorageError::Pool { message, .. }) => message,
            other => panic!(
                "reusing a cancelled writer handle must fail loudly with \
                 'connection already consumed'; got {other:?}"
            ),
        };
        assert!(
            message.contains("connection already consumed"),
            "expected the cancelled handle's reuse error to name the pinned \
             failure; got {message:?}"
        );

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("cancelled writer's detached SQLite call did not finish");
        assert!(cancelled, "writer batch task did not report cancellation");
    }

    /// Transaction-control statements (`BEGIN`/`START`/`COMMIT`/`END`/
    /// `ROLLBACK`/`SAVEPOINT`/`RELEASE`) in `execute_batch` input are rejected with a
    /// typed invalid-input error BEFORE anything executes, on the standalone
    /// path: a caller `COMMIT` inside the batch's own `BEGIN IMMEDIATE`
    /// would commit early and break the all-or-nothing contract. The
    /// rejection must leave the handle untouched and fully reusable, and no
    /// statement (not even the valid ones before the offending one) may
    /// have run.
    #[tokio::test]
    async fn execute_batch_rejects_transaction_control_before_executing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_tx_control_reject.db")),
            checkout_timeout: std::time::Duration::from_millis(250),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE tx_reject_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }

        let handle_slot = acquire_handle_slot(
            pool.sql_bridge_writer_slots(),
            pool.config().checkout_timeout,
            "sql_bridge.writer_handle",
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };

        for tail in ["COMMIT", "BEGIN"] {
            let multi = khive_storage::SqlWriter::execute_batch(
                &mut writer,
                vec![SqlStatement {
                    sql: format!(
                        "INSERT INTO tx_reject_test (id, val) VALUES (10, 'tail'); {tail}"
                    ),
                    params: vec![],
                    label: None,
                }],
            )
            .await;
            let message = multi
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                message.contains("Multiple statements"),
                "a SqlStatement with trailing {tail} must be rejected before execution; got {message}"
            );
        }

        // A valid INSERT first, then a bare COMMIT: the whole batch must be
        // rejected and the INSERT must NOT have run.
        let batch = khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![
                SqlStatement {
                    sql: "INSERT INTO tx_reject_test (id, val) VALUES (1, 'a')".into(),
                    params: vec![],
                    label: None,
                },
                SqlStatement {
                    sql: "COMMIT".into(),
                    params: vec![],
                    label: None,
                },
            ],
        )
        .await;
        match &batch {
            Err(StorageError::InvalidInput {
                operation, message, ..
            }) => {
                assert_eq!(operation.as_ref(), "execute_batch");
                assert!(
                    message.contains("transaction control") && message.contains("COMMIT"),
                    "the rejection must name the offending statement head; got {message:?}"
                );
            }
            other => {
                panic!("a batch containing a bare COMMIT must be rejected up front; got {other:?}")
            }
        }

        // Every transaction-control head is rejected, case-insensitively and
        // through leading whitespace and `--`/`/* */` comments.
        for sql in [
            "BEGIN IMMEDIATE",
            "START TRANSACTION",
            "commit",
            "End transaction",
            "ROLLBACK",
            "SAVEPOINT sp1",
            "RELEASE sp1",
            "  -- leading comment\nCOMMIT",
            "/* block */ rollback to savepoint sp1",
        ] {
            let rejected = khive_storage::SqlWriter::execute_batch(
                &mut writer,
                vec![SqlStatement {
                    sql: sql.into(),
                    params: vec![],
                    label: None,
                }],
            )
            .await;
            assert!(
                matches!(&rejected, Err(StorageError::InvalidInput { .. })),
                "transaction-control head {sql:?} must be rejected; got {rejected:?}"
            );
        }

        // The rejection ran before the handle was taken: no statement
        // executed (the INSERT above did not land), and the handle is still
        // fully reusable.
        let count: i64 = {
            let guard = pool.reader().unwrap();
            guard
                .conn()
                .query_row("SELECT COUNT(*) FROM tx_reject_test", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count, 0, "a rejected batch must not have executed anything");

        let affected = khive_storage::SqlWriter::execute(
            &mut writer,
            SqlStatement {
                sql: "INSERT INTO tx_reject_test (id, val) VALUES (2, 'b')".into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .expect("the handle must survive a rejected batch untouched");
        assert_eq!(affected, 1);
    }

    #[tokio::test]
    async fn execute_batch_rejects_multi_statement_on_pool_backed_path() {
        let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE multi_statement_pool_test (id INTEGER PRIMARY KEY, val TEXT)",
            )
            .unwrap();
        let bridge = SqlBridge::new(Arc::clone(&pool), false);
        let mut writer = bridge.writer().await.unwrap();

        let result = khive_storage::SqlWriter::execute_batch(
            &mut *writer,
            vec![SqlStatement {
                sql: "INSERT INTO multi_statement_pool_test (id, val) VALUES (1, 'x'); COMMIT"
                    .into(),
                params: vec![],
                label: None,
            }],
        )
        .await;
        let message = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            message.contains("Multiple statements"),
            "pool-backed execute_batch must reject a trailing COMMIT; got {message}"
        );
        let count: i64 = pool
            .reader()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM multi_statement_pool_test",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn inline_execute_batch_rejects_multi_statement_sql() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("sql_bridge_multi_statement_inline.db")),
                write_queue_enabled: Some(true),
                write_routing_strict: true,
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE multi_statement_inline_test (id INTEGER PRIMARY KEY, val TEXT)",
            )
            .unwrap();
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let result = bridge
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute_batch(vec![SqlStatement {
                            sql: "INSERT INTO multi_statement_inline_test (id, val) VALUES (1, 'x'); BEGIN"
                                .into(),
                            params: vec![],
                            label: None,
                        }])
                        .await
                        .map(|_| Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await;
        let message = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            message.contains("Multiple statements"),
            "InlineWriter must reject a trailing BEGIN; got {message}"
        );
        let count: i64 = pool
            .reader()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM multi_statement_inline_test",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Unit matrix for [`transaction_control_head`]: statement heads are
    /// classified case-insensitively through leading whitespace and
    /// comments; non-transaction-control heads (including identifiers that
    /// merely START with a keyword) never match.
    #[test]
    fn transaction_control_head_classification_matrix() {
        for (sql, expected) in [
            ("BEGIN", Some("BEGIN")),
            ("begin immediate", Some("BEGIN")),
            ("START TRANSACTION", Some("START")),
            ("start transaction", Some("START")),
            ("COMMIT", Some("COMMIT")),
            ("commit;", Some("COMMIT")),
            ("END", Some("END")),
            ("end transaction", Some("END")),
            ("ROLLBACK", Some("ROLLBACK")),
            ("rollback to savepoint sp1", Some("ROLLBACK")),
            ("SAVEPOINT sp1", Some("SAVEPOINT")),
            ("RELEASE sp1", Some("RELEASE")),
            ("release savepoint sp1", Some("RELEASE")),
            ("   \t COMMIT", Some("COMMIT")),
            ("\u{feff}BEGIN", Some("BEGIN")),
            ("-- a comment\nCOMMIT", Some("COMMIT")),
            // SQLite does not nest block comments: the comment ends at the
            // first `*/`, leaving `*/ COMMIT`, which is not a statement head.
            ("/* /* nested? no */ */ COMMIT", None),
            ("-- one\n-- two\n  /* x */ begin", Some("BEGIN")),
            ("INSERT INTO t VALUES (1)", None),
            ("UPDATE t SET x = 1", None),
            ("DELETE FROM t", None),
            ("SELECT * FROM commit_log", None),
            ("CREATE TABLE rollback_audit (id INTEGER)", None),
            ("/* comment only */", None),
            ("", None),
        ] {
            assert_eq!(
                transaction_control_head(sql),
                expected,
                "classification mismatch for {sql:?}"
            );
        }
    }

    #[test]
    fn sqlite_accepts_utf8_bom_before_transaction_control() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE bom_transaction_test (id INTEGER)")
            .unwrap();
        conn.execute_batch("\u{feff}BEGIN IMMEDIATE").unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
    }

    /// The queue-backed `execute_batch` path rejects transaction-control
    /// statements too, and the rejection protects the writer task: a caller
    /// `COMMIT` that reached the task would close its per-request `BEGIN
    /// IMMEDIATE` and terminate the task permanently. After the typed
    /// rejection, a legitimate batch must still succeed through the SAME
    /// writer task (it was never touched).
    #[tokio::test]
    async fn execute_batch_rejects_transaction_control_on_queue_backed_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_tx_reject_queue.db")),
            checkout_timeout: std::time::Duration::from_millis(250),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE tx_reject_queue_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();

        let rejected = khive_storage::SqlWriter::execute_batch(
            &mut *writer,
            vec![
                SqlStatement {
                    sql: "INSERT INTO tx_reject_queue_test (id, val) VALUES (1, 'a')".into(),
                    params: vec![],
                    label: None,
                },
                SqlStatement {
                    sql: "COMMIT".into(),
                    params: vec![],
                    label: None,
                },
            ],
        )
        .await;
        assert!(
            matches!(&rejected, Err(StorageError::InvalidInput { .. })),
            "a bare COMMIT in a queue-backed batch must be rejected up front; got {rejected:?}"
        );

        let affected = khive_storage::SqlWriter::execute_batch(
            &mut *writer,
            vec![SqlStatement {
                sql: "INSERT INTO tx_reject_queue_test (id, val) VALUES (2, 'b')".into(),
                params: vec![],
                label: None,
            }],
        )
        .await
        .expect("the writer task must survive the rejected batch");
        assert_eq!(affected, 1);

        let count: i64 = {
            let guard = pool.reader().unwrap();
            guard
                .conn()
                .query_row("SELECT COUNT(*) FROM tx_reject_queue_test", [], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(
            count, 1,
            "exactly the post-rejection batch's row may have landed"
        );
    }

    /// A failed ROLLBACK after a statement failure poisons the handle: the
    /// connection may be in an unknown transaction state, so it is dropped
    /// instead of restored, and every subsequent call on the same handle
    /// fails loudly with "connection already consumed". The caller sees the
    /// ORIGINAL statement error with the poison context attached (the
    /// rollback failure is never hidden, but never replaces the original).
    ///
    /// Forcing the arm legitimately (the pre-round-2 version smuggled a
    /// bare `COMMIT` into the batch, which `execute_batch` now rejects up
    /// front): a connection authorizer denies the `ROLLBACK` transaction
    /// operation, so the error path's `ROLLBACK` genuinely fails while the
    /// batch's own `BEGIN IMMEDIATE` and the statements run normally.
    #[tokio::test]
    async fn failed_rollback_poisons_handle_reuse_fails_loud() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

        fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
            match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_rollback_poison.db")),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE rollback_poison_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }

        let handle_slot = acquire_handle_slot(
            pool.sql_bridge_writer_slots(),
            pool.config().checkout_timeout,
            "sql_bridge.writer_handle",
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        conn.authorizer(Some(deny_rollback)).unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };

        let batch = khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![
                SqlStatement {
                    sql: "INSERT INTO rollback_poison_test (id, val) VALUES (1, 'a')".into(),
                    params: vec![],
                    label: None,
                },
                SqlStatement {
                    sql: "SELECT FROM WHERE".into(),
                    params: vec![],
                    label: None,
                },
            ],
        )
        .await;
        let batch_error = batch.expect_err("invalid second statement must fail the batch");
        let poison = match &batch_error {
            StorageError::Driver { source, .. } => source
                .downcast_ref::<PoisonedBatchError>()
                .expect("failed rollback must retain its typed poison wrapper"),
            other => panic!("failed rollback must return a driver error; got {other:?}"),
        };
        assert!(
            matches!(&poison.poison_reason, BatchPoisonReason::RollbackFailed(_)),
            "the poison cause must be compiler-checked as RollbackFailed; got {poison:?}"
        );
        let batch_message = batch_error.to_string();
        assert!(
            batch_message.contains("ROLLBACK after statement failure failed"),
            "the caller must see the poison context naming the failed \
             rollback; got {batch_message:?}"
        );
        assert!(
            batch_message.contains("original error"),
            "the original statement error must stay visible alongside the \
             poison context; got {batch_message:?}"
        );

        let reuse = khive_storage::SqlWriter::execute(
            &mut writer,
            SqlStatement {
                sql: "CREATE TABLE rollback_poison_probe (id INTEGER PRIMARY KEY)".into(),
                params: vec![],
                label: None,
            },
        )
        .await;
        let message = match reuse {
            Err(StorageError::Pool { message, .. }) => message,
            other => panic!(
                "reusing a poisoned writer handle must fail loudly with \
                 'connection already consumed'; got {other:?}"
            ),
        };
        assert!(
            message.contains("connection already consumed"),
            "expected the poisoned handle's reuse error to name the pinned \
             failure; got {message:?}"
        );
    }

    /// A NON-TRANSIENT `BEGIN IMMEDIATE` failure poisons the handle instead
    /// of restoring it: the connection's transaction state is suspect (here
    /// a caller-driven transaction is already open on the same connection,
    /// so SQLite answers "cannot start a transaction within a transaction"),
    /// and the returned error carries the poison context.
    #[tokio::test]
    async fn non_transient_begin_failure_poisons_handle() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_begin_poison.db")),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());

        let handle_slot = acquire_handle_slot(
            pool.sql_bridge_writer_slots(),
            pool.config().checkout_timeout,
            "sql_bridge.writer_handle",
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        // A caller-driven open transaction on the same connection: the
        // batch's own `BEGIN IMMEDIATE` fails non-transiently.
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };

        let batch = khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            }],
        )
        .await;
        let batch_error = batch.expect_err("BEGIN inside an open transaction must fail");
        let poison = match &batch_error {
            StorageError::Driver { source, .. } => source
                .downcast_ref::<PoisonedBatchError>()
                .expect("failed BEGIN must retain its typed poison wrapper"),
            other => panic!("failed BEGIN must return a driver error; got {other:?}"),
        };
        assert!(
            matches!(&poison.poison_reason, BatchPoisonReason::BeginFailed),
            "the poison cause must be compiler-checked as BeginFailed; got {poison:?}"
        );
        let batch_message = batch_error.to_string();
        assert!(
            batch_message.contains("BEGIN IMMEDIATE failed non-transiently"),
            "a non-transient BEGIN failure must surface the poison context; \
             got {batch_message:?}"
        );
        assert!(
            batch_message.contains("cannot start a transaction within a transaction"),
            "the original BEGIN error must stay visible; got {batch_message:?}"
        );

        let reuse = khive_storage::SqlWriter::execute(
            &mut writer,
            SqlStatement {
                sql: "CREATE TABLE begin_poison_probe (id INTEGER PRIMARY KEY)".into(),
                params: vec![],
                label: None,
            },
        )
        .await;
        assert!(
            matches!(
                &reuse,
                Err(StorageError::Pool { message, .. })
                    if message.contains("connection already consumed")
            ),
            "a handle poisoned by a non-transient BEGIN failure must be \
             dropped, not restored; got {reuse:?}"
        );
    }

    /// A BUSY/LOCKED `BEGIN IMMEDIATE` failure is transient contention: the
    /// connection itself is untouched, so the handle is restored as
    /// reusable, and the next call succeeds once the contending lock is
    /// released.
    #[tokio::test]
    async fn busy_begin_failure_restores_handle_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_begin_busy.db")),
            checkout_timeout: std::time::Duration::from_millis(250),
            busy_timeout: std::time::Duration::from_millis(100),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch("CREATE TABLE begin_busy_test (id INTEGER PRIMARY KEY)")
                .unwrap();
        }

        // Hold SQLite's write lock from a separate connection so the batch's
        // `BEGIN IMMEDIATE` genuinely fails with SQLITE_BUSY after the short
        // busy timeout.
        let lock_conn = pool.open_standalone_writer().unwrap();
        lock_conn.execute_batch("BEGIN IMMEDIATE").unwrap();

        let handle_slot = acquire_handle_slot(
            pool.sql_bridge_writer_slots(),
            pool.config().checkout_timeout,
            "sql_bridge.writer_handle",
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _slot: handle_slot,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };

        let batch = khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![SqlStatement {
                sql: "INSERT INTO begin_busy_test (id) VALUES (1)".into(),
                params: vec![],
                label: None,
            }],
        )
        .await;
        let batch_error = batch.expect_err("BEGIN IMMEDIATE under a held write lock must fail");
        assert!(
            batch_error.to_string().contains("database is locked"),
            "the busy BEGIN failure must surface SQLite's busy error; got {batch_error:?}"
        );

        lock_conn.execute_batch("ROLLBACK").unwrap();
        drop(lock_conn);

        let affected = khive_storage::SqlWriter::execute(
            &mut writer,
            SqlStatement {
                sql: "INSERT INTO begin_busy_test (id) VALUES (2)".into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .expect("a busy BEGIN failure must restore the handle as reusable");
        assert_eq!(affected, 1);
    }

    /// The manual `atomic_unit` path (write queue off) shares the pool's
    /// one-permit writer-handle budget with `writer()`: while a boxed writer
    /// handle is live, `atomic_unit` times out; after the handle drops, the
    /// next `atomic_unit` succeeds on the same pool.
    #[tokio::test]
    async fn manual_atomic_unit_shares_writer_permit_budget_with_writer_handle() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_atomic_unit_budget.db")),
            checkout_timeout: std::time::Duration::from_millis(50),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS atomic_unit_budget_test \
                     (id INTEGER PRIMARY KEY, val INTEGER NOT NULL)",
                )
                .unwrap();
        }

        fn insert_op(id: i64) -> AtomicUnitOp {
            Box::new(move |writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO atomic_unit_budget_test (id, val) VALUES (?1, ?2)"
                                .into(),
                            params: vec![SqlValue::Integer(id), SqlValue::Integer(id)],
                            label: None,
                        })
                        .await
                        .map_err(|e| {
                            khive_storage::StorageError::driver(
                                StorageCapability::Sql,
                                "atomic_unit_budget_test_insert",
                                e,
                            )
                        })?;
                    Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                })
            })
        }

        let writer_handle = bridge.writer().await.unwrap();
        let blocked = bridge.atomic_unit(insert_op(1)).await;
        assert!(
            matches!(
                &blocked,
                Err(StorageError::Timeout { operation })
                    if operation.as_ref() == "sql_bridge.atomic_unit_handle"
            ),
            "atomic_unit must time out on the shared writer permit while a \
             writer handle is live; got {blocked:?}"
        );

        drop(writer_handle);
        let unblocked = bridge.atomic_unit(insert_op(2)).await;
        assert!(
            unblocked.is_ok(),
            "atomic_unit must succeed once the writer handle releases the \
             shared writer permit; got {unblocked:?}"
        );

        let mut reader = bridge.reader().await.unwrap();
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM atomic_unit_budget_test".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(1))),
            "only the post-drop atomic_unit call may have committed; got {count:?}"
        );
    }

    /// ADR-067 Component A entry 10: with `KHIVE_WRITE_QUEUE=1`,
    /// `SqliteWriter::execute_batch` (reached via `SqlBridge::writer()`)
    /// routes the whole statement list through the WriterTask channel
    /// instead of opening its own `BEGIN IMMEDIATE` on the standalone
    /// connection, and the row is actually committed and readable back.
    #[tokio::test]
    async fn execute_batch_routes_through_writer_task_when_flag_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write_queue_execute_batch.db");
        let config = PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS write_queue_batch_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }

        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let mut writer = bridge.writer().await.unwrap();
        let affected = writer
            .execute_batch(vec![
                SqlStatement {
                    sql: "INSERT INTO write_queue_batch_test (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(1), SqlValue::Text("a".into())],
                    label: None,
                },
                SqlStatement {
                    sql: "INSERT INTO write_queue_batch_test (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(2), SqlValue::Text("b".into())],
                    label: None,
                },
            ])
            .await
            .unwrap();
        assert_eq!(affected, 2);

        let mut reader = bridge.reader().await.unwrap();
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM write_queue_batch_test".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(2))),
            "expected 2 rows, got {count:?}"
        );
        assert_eq!(
            pool.writer_task_spawn_count(),
            1,
            "the flag-ON path must actually spawn and use the writer task"
        );
    }

    /// ADR-067 Component A entry 10, atomicity: a batch whose second
    /// statement fails (duplicate primary key) must roll back the WHOLE
    /// request — including the first statement's otherwise-successful
    /// INSERT — because the WriterTask commits or rolls back one
    /// `WriteRequest` as a single unit (ADR-067 Component A). Zero rows must
    /// land, not one.
    #[tokio::test]
    async fn execute_batch_rolls_back_atomically_on_mid_sequence_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write_queue_execute_batch_rollback.db");
        let config = PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS write_queue_rollback_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }

        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let mut writer = bridge.writer().await.unwrap();
        let result = writer
            .execute_batch(vec![
                // Statement 1: succeeds on its own.
                SqlStatement {
                    sql: "INSERT INTO write_queue_rollback_test (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(1), SqlValue::Text("first".into())],
                    label: None,
                },
                // Statement 2: duplicate primary key — fails mid-sequence.
                SqlStatement {
                    sql: "INSERT INTO write_queue_rollback_test (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(1), SqlValue::Text("duplicate".into())],
                    label: None,
                },
                // Statement 3: never reached.
                SqlStatement {
                    sql: "INSERT INTO write_queue_rollback_test (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(2), SqlValue::Text("third".into())],
                    label: None,
                },
            ])
            .await;
        assert!(
            result.is_err(),
            "a batch with a mid-sequence PK conflict must return an error"
        );

        let mut reader = bridge.reader().await.unwrap();
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM write_queue_rollback_test".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(0))),
            "the whole request must roll back — including statement 1's \
             otherwise-successful INSERT — not just the failing statement; \
             got {count:?}"
        );
    }

    /// ADR-067 Component A: before
    /// this fix, `block_on_sync` (this file) `unreachable!()`-panicked if
    /// an `atomic_unit` closure's future was `Pending` on its first poll.
    /// That panic ran inside the writer task's own `spawn_blocking` frame
    /// (see `atomic_unit`'s flag-on branch), and `run_writer_task` treats
    /// any `spawn_blocking` `JoinError` as fatal — the whole writer task
    /// exits, taking down every subsequent write for this pool. Proves the
    /// fix: an `atomic_unit` op built to suspend on first poll (via
    /// `std::future::pending`, never actually resolving) now returns a
    /// clean `Err` from `atomic_unit` — no panic — AND the writer task
    /// survives to serve a completely unrelated, well-behaved `atomic_unit`
    /// call immediately afterward.
    ///
    /// Not `#[serial]` / no env var: builds the pool directly with
    /// `write_queue_enabled: Some(true)` in the `PoolConfig` literal, same
    /// technique as this round's other new routing tests.
    #[tokio::test]
    async fn atomic_unit_pending_future_errors_without_killing_writer_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic_unit_pending_future.db");
        let config = PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS atomic_unit_pending_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }
        assert!(
            pool.writer_task_handle().unwrap().is_some(),
            "writer task must be spawned with the flag on for a file-backed pool"
        );

        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        // A closure whose future never resolves on first poll — the exact
        // misuse `block_on_sync` must reject instead of panicking on.
        let pending_op: AtomicUnitOp = Box::new(|_writer| {
            Box::pin(std::future::pending::<
                khive_storage::types::StorageResult<Box<dyn std::any::Any + Send>>,
            >())
        });

        let pending_result = bridge.atomic_unit(pending_op).await;
        assert!(
            pending_result.is_err(),
            "a Pending-on-first-poll atomic_unit closure must return Err, \
             not panic; got {pending_result:?}"
        );

        // If the panic had instead killed the writer task, every subsequent
        // write on this pool (including a completely unrelated, correctly
        // non-blocking atomic_unit call) would now fail with a channel-closed
        // error. Prove the task is still alive and serving requests.
        let ok_op: AtomicUnitOp = Box::new(|writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO atomic_unit_pending_test (id, val) VALUES (?1, ?2)"
                            .into(),
                        params: vec![SqlValue::Integer(1), SqlValue::Text("survived".into())],
                        label: None,
                    })
                    .await
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            StorageCapability::Sql,
                            "atomic_unit_pending_future_test_insert",
                            e,
                        )
                    })?;
                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
            })
        });
        let ok_result = bridge.atomic_unit(ok_op).await;
        assert!(
            ok_result.is_ok(),
            "writer task must survive a Pending misuse and keep serving \
             subsequent well-behaved atomic_unit requests; got {ok_result:?}"
        );

        let mut reader = bridge.reader().await.unwrap();
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM atomic_unit_pending_test".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(1))),
            "the well-behaved atomic_unit call after the Pending misuse must \
             have actually committed its write; got {count:?}"
        );
    }

    /// ADR-136 D1 gate 1/3: with `KHIVE_WRITE_ROUTING=strict` and no writer
    /// task available, `SqlBridge::writer()` must error instead of silently
    /// degrading to a standalone connection — even when the reason no handle
    /// exists is simply that the queue itself was never enabled. Strict
    /// routing without an enabled queue is a caller misconfiguration this
    /// gate refuses rather than silently no-ops: an operator who set
    /// `KHIVE_WRITE_ROUTING=strict` believing every write is single-admission
    /// must be told loudly if `KHIVE_WRITE_QUEUE` was never turned on, not
    /// left thinking strict routing is in effect when it is not.
    #[tokio::test]
    async fn writer_strict_routing_fails_closed_without_writer_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strict_writer.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            write_routing_strict: true,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let result = bridge.writer().await;
        let err = match result {
            Ok(_) => panic!(
                "KHIVE_WRITE_ROUTING=strict with no writer task must fail closed, not \
                 silently degrade to a standalone connection"
            ),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("strict"),
            "error must name strict routing, got: {err}"
        );
    }

    /// ADR-136 D1 gate 3/4: with `KHIVE_WRITE_ROUTING=strict` and no writer
    /// task available, `SqlBridge::atomic_unit` must error instead of
    /// silently falling back to a manual `BEGIN IMMEDIATE` on a standalone
    /// connection.
    #[tokio::test]
    async fn atomic_unit_strict_routing_fails_closed_without_writer_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strict_atomic_unit.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            write_routing_strict: true,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let op: AtomicUnitOp = Box::new(|_writer| {
            Box::pin(async move { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
        });
        let result = bridge.atomic_unit(op).await;
        assert!(
            result.is_err(),
            "KHIVE_WRITE_ROUTING=strict but the queue is off (no writer task handle) must \
             fail closed instead of falling back to a manual BEGIN IMMEDIATE; got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("strict"),
            "error must name strict routing, got: {msg}"
        );
    }

    /// ADR-136 D1 gate 3 amendment: production DOES read through a
    /// queue-backed `writer()` handle — `khive-pack-comm`'s handlers obtain
    /// a writer then call `w.query_row(...)` cursor-style, and
    /// `khive-pack-gtd`'s bootstrap calls `w.query_all("PRAGMA
    /// table_info...")` on one. Exercise that exact shape under a strict,
    /// queue-enabled pool: write through the handle, then read the same row
    /// back through it before it is dropped.
    #[tokio::test]
    async fn writer_handle_supports_read_after_write_under_strict_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_read_after_write.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS writer_cursor_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }

        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let mut w = bridge.writer().await.unwrap();
        w.execute(SqlStatement {
            sql: "INSERT INTO writer_cursor_test (id, val) VALUES (?1, ?2)".into(),
            params: vec![SqlValue::Integer(1), SqlValue::Text("via-writer".into())],
            label: None,
        })
        .await
        .unwrap();

        let row = w
            .query_row(SqlStatement {
                sql: "SELECT val FROM writer_cursor_test WHERE id = ?1".into(),
                params: vec![SqlValue::Integer(1)],
                label: None,
            })
            .await
            .unwrap()
            .expect("row inserted through the same writer handle must be visible to it");
        assert!(
            matches!(&row.columns[0].value, SqlValue::Text(v) if v == "via-writer"),
            "query_row through a queue-backed writer handle must see its own \
             committed write; got {:?}",
            row.columns[0].value
        );
    }

    /// Queue-backed reads charge the READER permit budget (`ensure_conn`
    /// acquires `sql_bridge_reader_slots`), and a cancelled queue-backed
    /// read is followed by a successful lazy reopen — the documented
    /// contrast with standalone handles, whose consumed connection makes
    /// every later call fail. Arm 1 saturates the one reader permit and
    /// asserts the queue-backed read times out on the READER budget (not
    /// the writer budget). Arm 2 aborts an in-flight read and asserts the
    /// next read on the same handle succeeds by reopening.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_backed_read_uses_reader_budget_and_reopens_after_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue_backed_reader_budget.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS reopen_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
                )
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let mut w = bridge.writer().await.unwrap();
        w.execute(SqlStatement {
            sql: "INSERT INTO reopen_test (id, val) VALUES (1, 'seed')".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();

        // Arm 1: with the sole reader permit held, the queue-backed read
        // must time out on the reader budget.
        let held = pool
            .sql_bridge_reader_slots()
            .acquire_owned()
            .await
            .unwrap();
        let starved = w
            .query_row(SqlStatement {
                sql: "SELECT val FROM reopen_test WHERE id = 1".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &starved,
                Err(StorageError::Timeout { operation })
                    if operation.as_ref() == "sql_bridge.reader_handle"
            ),
            "queue-backed read with reader permits saturated must time out \
             on the reader budget; got {starved:?}"
        );
        drop(held);

        // Arm 2: a queue-backed handle in the exact post-cancelled-read
        // state — `handle: None` because the cancelled call took the boxed
        // connection out and never returned it — must serve the next read by
        // lazily reopening, never a hard "connection already consumed"
        // failure (that contract is standalone-only; the doc names the
        // contrast). Constructed directly so the state is deterministic
        // rather than racing an abort against ensure_conn.
        let writer_task = pool
            .writer_task_handle()
            .expect("queue-enabled file pool must offer a writer task")
            .expect("writer task present under write_queue_enabled");
        let mut post_cancel = SqliteWriter {
            handle: None,
            writer_task: Some(writer_task),
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        let row = post_cancel
            .query_row(SqlStatement {
                sql: "SELECT val FROM reopen_test WHERE id = 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("read on a queue-backed handle with no resident connection must reopen")
            .expect("seeded row must be visible");
        assert!(
            matches!(&row.columns[0].value, SqlValue::Text(v) if v == "seed"),
            "reopened read must return the seeded row; got {:?}",
            row.columns[0].value
        );
    }

    /// Cancelling an actual IN-FLIGHT queue-backed read (not a constructed
    /// post-cancel state): the detached blocking task keeps running the
    /// query, holding the lazily opened connection and its reader permit
    /// until SQLite finishes. With a single reader permit, the next read's
    /// reopen therefore cannot succeed while the detached read still holds
    /// the permit — it times out on the reader budget (a typed `Timeout`,
    /// not the hard "connection already consumed" failure of standalone
    /// handles) — and succeeds once the detached read completes and
    /// releases the permit. This pins the documented reopen claim together
    /// with its permit-contention boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_inflight_queue_backed_read_reopens_after_detached_read_completes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue_backed_inflight_cancel.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS inflight_cancel_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL);
                     INSERT INTO inflight_cancel_test (id, val) VALUES (1, 'seed');",
                )
                .unwrap();
        }

        let writer_task = pool
            .writer_task_handle()
            .expect("queue-enabled file pool must offer a writer task")
            .expect("writer task present under write_queue_enabled");
        let writer = Arc::new(tokio::sync::Mutex::new(SqliteWriter {
            handle: None,
            writer_task: Some(writer_task),
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        }));

        // A real first read through the queue-backed handle: lazily opens
        // and retains the read-only connection under the sole reader permit.
        writer
            .lock()
            .await
            .query_row(SqlStatement {
                sql: "SELECT val FROM inflight_cancel_test WHERE id = 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap()
            .expect("seeded row must be visible");

        // Install the blocking progress gate on the resident connection so
        // the next read is genuinely in flight when it is aborted.
        let (entered, release, completed) = {
            let mut w = writer.lock().await;
            let handle = w.handle.take().expect("first read must retain the handle");
            let gate = blocking_progress_gate(&handle.conn);
            w.handle = Some(handle);
            gate
        };

        let writer_clone = Arc::clone(&writer);
        let read = tokio::spawn(async move {
            writer_clone
                .lock()
                .await
                .query_row(progress_gate_statement())
                .await
        });
        entered.notified().await;
        read.abort();
        assert!(
            matches!(&read.await, Err(error) if error.is_cancelled()),
            "the in-flight queue-backed read must be cancellable"
        );

        // The detached blocking task still holds the sole reader permit, so
        // the reopen must time out on the reader budget — typed, not the
        // hard "connection already consumed" failure of standalone handles.
        let blocked = writer
            .lock()
            .await
            .query_row(SqlStatement {
                sql: "SELECT val FROM inflight_cancel_test WHERE id = 1".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &blocked,
                Err(StorageError::Timeout { operation })
                    if operation.as_ref() == "sql_bridge.reader_handle"
            ),
            "while the detached cancelled read holds the last reader permit, \
             the reopen must time out on the reader budget; got {blocked:?}"
        );

        // Release the gate: the detached read finishes and releases the permit.
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), completed.notified())
            .await
            .expect("the detached cancelled read did not complete");

        // Now the reopen on the same handle succeeds.
        let row = writer
            .lock()
            .await
            .query_row(SqlStatement {
                sql: "SELECT val FROM inflight_cancel_test WHERE id = 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("after the detached cancelled read completes, the reopen must succeed")
            .expect("seeded row must be visible");
        assert!(
            matches!(&row.columns[0].value, SqlValue::Text(v) if v == "seed"),
            "reopened read must return the seeded row; got {:?}",
            row.columns[0].value
        );
    }

    /// ADR-136 D1 gate 3 amendment: `SqlWriter::query_row`/`query_all` carry
    /// no read-only restriction at the trait level — a caller could hand a
    /// DML-with-RETURNING statement to `query_row` expecting it to behave
    /// like any other query. Under a queue-backed handle, the standalone
    /// connection `SqliteWriter::ensure_conn` lazily opens must be
    /// read-only, so SQLite rejects the statement outright instead of
    /// quietly mutating the row on an untracked connection outside the
    /// `WriterTask`. Red-proof: reverting `ensure_conn` to
    /// `open_standalone_writer` makes this test fail (the UPDATE succeeds
    /// and mutates the row).
    #[tokio::test]
    async fn writer_query_row_rejects_dml_with_returning_on_queue_backed_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_readonly_returning.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS writer_returning_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL);
                     INSERT INTO writer_returning_test (id, val) VALUES (1, 'original');",
                )
                .unwrap();
        }

        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let mut w = bridge.writer().await.unwrap();
        let result = w
            .query_row(SqlStatement {
                sql: "UPDATE writer_returning_test SET val = 'mutated' \
                      WHERE id = ?1 RETURNING val"
                    .into(),
                params: vec![SqlValue::Integer(1)],
                label: None,
            })
            .await;
        assert!(
            result.is_err(),
            "a DML-with-RETURNING statement through query_row on a \
             queue-backed writer handle must be rejected, not executed on \
             an untracked read-write connection; got {result:?}"
        );

        let mut reader = bridge.reader().await.unwrap();
        let val = reader
            .query_scalar(SqlStatement {
                sql: "SELECT val FROM writer_returning_test WHERE id = ?1".into(),
                params: vec![SqlValue::Integer(1)],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(&val, Some(SqlValue::Text(v)) if v == "original"),
            "the rejected UPDATE...RETURNING must not have altered the row; got {val:?}"
        );
    }

    /// ADR-136 D1 acceptance arm: a 5-op batch shaped like `[send, mark,
    /// mark, mark, mark]` at the storage layer — every "mark" op is a real
    /// `UPDATE` against its own pre-seeded row, so (like `send`) it routes
    /// through the writer task rather than bypassing it as a `SELECT` would
    /// — issued while 3 concurrent writers contend the write path, must
    /// complete every op — no checkout timeout — once routing is strict and
    /// the queue is on. An occupier holds the writer task's single drain
    /// slot until all 8 requests (3 contenders + send + 4 marks) are
    /// provably enqueued behind it (`queue_depth() >= 8`, the same
    /// occupier/`queue_depth()` discriminator the migrated-call-site tests
    /// use), so this proves genuine contention instead of a scheduler that
    /// happens to drain the tiny writes before the others even enqueue.
    /// Mirrors the measured production failure ADR-136's Context section
    /// documents (middle ops of a batch starving while a sibling write wins
    /// under the legacy fixed-deadline pool mutex). Red-proofed: reverting
    /// the marks back to `bridge.reader()` `SELECT`s (the pre-fix shape)
    /// makes the `queue_depth() >= 8` wait time out and fail, since a read
    /// never reaches the writer task's channel — confirming this version
    /// actually requires all four marks to be real writes.
    #[tokio::test]
    async fn acceptance_five_op_batch_completes_under_concurrent_write_contention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acceptance_batch.db");
        let config = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS acceptance_batch \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL);
                     INSERT INTO acceptance_batch (id, val) VALUES \
                     (200, 'seed-0'), (201, 'seed-1'), (202, 'seed-2'), (203, 'seed-3');",
                )
                .unwrap();
        }

        let bridge = Arc::new(SqlBridge::new(Arc::clone(&pool), true));

        let writer_task = pool
            .writer_task_handle()
            .unwrap()
            .expect("writer task must be spawned for a file-backed pool with the flag on");

        // Occupier: holds the single writer-task drain slot until released,
        // so every op below is provably queued behind it rather than racing
        // to finish before the others even enqueue (same technique as
        // `rename_namespace_routes_through_writer_task_when_flag_enabled` in
        // `stores::text_tests`).
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), StorageError>(())
                    })
                    .await
            })
        };
        started_rx
            .await
            .expect("occupier must signal it has started running inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "channel must start empty once the occupier has been dequeued and is running"
        );

        // 3 concurrent writers contending the write path — each a
        // self-contained `execute()` through `SqlBridge::writer()`, matching
        // the "several short acquisitions" shape ADR-136's Context section
        // measures (a logical write is many short holds, not one long one).
        let contenders: Vec<_> = (0..3)
            .map(|i| {
                let bridge = Arc::clone(&bridge);
                tokio::spawn(async move {
                    let mut writer = bridge.writer().await?;
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO acceptance_batch (id, val) VALUES (?1, ?2)".into(),
                            params: vec![
                                SqlValue::Integer(100 + i),
                                SqlValue::Text(format!("contender-{i}")),
                            ],
                            label: None,
                        })
                        .await
                })
            })
            .collect();

        // The 5-op batch: [send, mark, mark, mark, mark].
        let send = {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move {
                let mut writer = bridge.writer().await?;
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO acceptance_batch (id, val) VALUES (?1, ?2)".into(),
                        params: vec![SqlValue::Integer(1), SqlValue::Text("send".into())],
                        label: None,
                    })
                    .await
            })
        };
        let marks: Vec<_> = (0..4)
            .map(|i| {
                let bridge = Arc::clone(&bridge);
                tokio::spawn(async move {
                    let mut writer = bridge.writer().await?;
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE acceptance_batch SET val = ?2 WHERE id = ?1".into(),
                            params: vec![
                                SqlValue::Integer(200 + i),
                                SqlValue::Text(format!("marked-{i}")),
                            ],
                            label: None,
                        })
                        .await
                })
            })
            .collect();

        // All 8 requests must actually reach the writer task's channel
        // while the occupier still holds the single drain slot.
        let mut saw_all_enqueued = false;
        for _ in 0..200 {
            if writer_task.queue_depth() >= 8 {
                saw_all_enqueued = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            saw_all_enqueued,
            "not all 8 contending writes (3 contenders + send + 4 marks) reached \
             the writer task's channel while the occupier held the single drain \
             slot — got depth {}",
            writer_task.queue_depth()
        );

        release_tx
            .send(())
            .expect("occupier must still be waiting on the release signal");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");

        for c in contenders {
            c.await
                .expect("contender task must not panic")
                .expect("contender write must complete without a checkout timeout");
        }
        send.await
            .expect("send task must not panic")
            .expect("send op must complete without a checkout timeout");
        for (i, m) in marks.into_iter().enumerate() {
            let affected = m
                .await
                .expect("mark task must not panic")
                .expect("mark op must complete without a checkout timeout — no starvation");
            assert_eq!(
                affected, 1,
                "mark {i} must have updated exactly its own row"
            );
        }

        let mut reader = bridge.reader().await.unwrap();
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM acceptance_batch".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(8))),
            "the 4 seeded mark rows plus 3 contenders plus the batch's own send \
             must all be present; got {count:?}"
        );

        for i in 0..4i64 {
            let mut reader = bridge.reader().await.unwrap();
            let val = reader
                .query_scalar(SqlStatement {
                    sql: "SELECT val FROM acceptance_batch WHERE id = ?1".into(),
                    params: vec![SqlValue::Integer(200 + i)],
                    label: None,
                })
                .await
                .unwrap();
            assert!(
                matches!(&val, Some(SqlValue::Text(v)) if *v == format!("marked-{i}")),
                "mark row {i} must reflect the persisted UPDATE after release; got {val:?}"
            );
        }
    }

    #[tokio::test]
    async fn file_backed_bridge_counts_writer_and_flag_off_atomic_unit_acquisitions() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("bridge_writer_acquisitions.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let before = pool.writer_acquisition_snapshot();

        drop(bridge.writer().await.unwrap());
        let after_writer = pool.writer_acquisition_snapshot();
        assert_eq!(
            after_writer.standalone_acquisitions,
            before.standalone_acquisitions + 1
        );
        assert_eq!(after_writer.acquisitions, before.acquisitions + 1);
        assert_eq!(after_writer.pooled_acquisitions, before.pooled_acquisitions);
        assert_eq!(
            after_writer.writer_task_acquisitions,
            before.writer_task_acquisitions
        );

        let op: AtomicUnitOp = Box::new(|_writer| {
            Box::pin(async { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
        });
        bridge.atomic_unit(op).await.unwrap();

        let after_atomic_unit = pool.writer_acquisition_snapshot();
        assert_eq!(
            after_atomic_unit.standalone_acquisitions,
            before.standalone_acquisitions + 2
        );
        assert_eq!(after_atomic_unit.acquisitions, before.acquisitions + 2);
        assert_eq!(
            after_atomic_unit.pooled_acquisitions,
            before.pooled_acquisitions
        );
        assert_eq!(
            after_atomic_unit.writer_task_acquisitions,
            before.writer_task_acquisitions
        );
    }
}
