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

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use khive_storage::error::StorageError;
use khive_storage::types::{PageRequest, SqlColumn, SqlRow, SqlStatement, SqlValue};
use khive_storage::{AtomicUnitOp, StorageCapability};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, StandaloneReaderPurpose};

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
            Err(_) => prepared.push(PreparedBatchStatement::PrepareAtExecution),
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
/// or roll back early and break the batch's all-or-nothing contract. Cached
/// read-only handles use the same lexical classification to drive their
/// separately admitted single-level read-transaction state machine below.
/// `START` is classified as the alternate transaction-opening spelling so
/// callers get a typed boundary error; `END` is SQLite's `COMMIT` spelling.
const TRANSACTION_CONTROL_KEYWORDS: [&str; 7] = [
    "BEGIN",
    "START",
    "COMMIT",
    "END",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
];

/// Skip the same leading whitespace, UTF-8 BOMs, empty statements (`;`), and
/// line/block comments SQLite accepts before an executable statement.
fn skip_sqlite_empty_prefix(mut rest: &[u8]) -> &[u8] {
    loop {
        let mut idx = 0;
        while idx < rest.len() && rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        rest = &rest[idx..];
        if let Some(tail) = rest.strip_prefix(b"\xEF\xBB\xBF") {
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b";") {
            rest = tail;
            continue;
        }
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
    rest
}

/// Return one ASCII SQL token after SQLite whitespace/comments/BOM trivia.
/// Empty-statement separators are deliberately not trivia here: callers use
/// this only after the executable statement head has already been consumed.
fn next_sqlite_token(mut rest: &[u8]) -> Option<(&[u8], &[u8])> {
    loop {
        let mut idx = 0;
        while idx < rest.len() && rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        rest = &rest[idx..];
        if let Some(tail) = rest.strip_prefix(b"\xEF\xBB\xBF") {
            rest = tail;
            continue;
        }
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

    let len = rest
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    (len != 0).then_some((&rest[..len], &rest[len..]))
}

/// Return a transaction-control keyword and the bytes following it, if any.
/// Matching is case-insensitive and requires a word boundary, so an identifier
/// that merely starts with `begin` or `commit` never matches.
fn transaction_control_parts(sql: &str) -> Option<(&'static str, &[u8])> {
    let rest = skip_sqlite_empty_prefix(sql.as_bytes());
    TRANSACTION_CONTROL_KEYWORDS
        .iter()
        .copied()
        .find_map(|keyword| {
            let kw = keyword.as_bytes();
            if rest.len() < kw.len() || !rest[..kw.len()].eq_ignore_ascii_case(kw) {
                return None;
            }
            let boundary = match rest.get(kw.len()) {
                Some(next) => !(next.is_ascii_alphanumeric() || *next == b'_'),
                None => true,
            };
            boundary.then_some((keyword, &rest[kw.len()..]))
        })
}

/// Return the transaction-control keyword heading `sql`, if any.
fn transaction_control_head(sql: &str) -> Option<&'static str> {
    transaction_control_parts(sql).map(|(keyword, _)| keyword)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CachedReadTransactionControl {
    /// `BEGIN`, `BEGIN TRANSACTION`, or their explicitly `DEFERRED` form.
    BeginDeferred,
    /// A transaction-ending control statement and its diagnostic keyword.
    Finish(&'static str),
    /// Transaction control that cannot be represented by the single-level
    /// admitted read-transaction state machine.
    Unsupported(&'static str),
}

/// Classify transaction control for a cached read-only connection.
///
/// A cached reader may own exactly one top-level deferred read transaction.
/// Immediate/exclusive starts could reserve write-side locks, while nested
/// savepoint/rollback-to controls would require a second lifecycle level, so
/// both remain rejected. Batch/write paths continue using
/// [`transaction_control_head`] and reject every variant without exception.
fn cached_read_transaction_control(sql: &str) -> Option<CachedReadTransactionControl> {
    let (keyword, tail) = transaction_control_parts(sql)?;
    match keyword {
        "BEGIN" => {
            // Accept exactly `BEGIN`, `BEGIN TRANSACTION`, `BEGIN DEFERRED`,
            // or `BEGIN DEFERRED TRANSACTION`, with no trailing tokens.
            // SQLite's grammar also admits `BEGIN TRANSACTION <name>` (the
            // name parses as an identifier and is ignored), so a mode keyword
            // in that trailing position — `BEGIN TRANSACTION IMMEDIATE` —
            // still parses, and classifying it by its first token alone would
            // launder what reads as a write-reserving start into a deferred
            // one. Every trailing token is therefore Unsupported — and the
            // check cannot stop at `next_sqlite_token` returning `None`,
            // because that tokenizer returns `None` for any non-identifier
            // byte, not only end-of-input: a quoted or bracketed tail
            // (`BEGIN TRANSACTION "IMMEDIATE"`, `[IMMEDIATE]`) would fall
            // out of the loop and read as the end of an accepted form. After
            // the accepted keywords, the remainder must reduce to nothing
            // under the same trivia/empty-statement skipping SQLite applies
            // (whitespace, comments, `;`), or the statement is Unsupported.
            let mut rest = tail;
            let mut saw_deferred = false;
            let mut saw_transaction = false;
            while let Some((token, next)) = next_sqlite_token(rest) {
                if !saw_deferred && !saw_transaction && token.eq_ignore_ascii_case(b"DEFERRED") {
                    saw_deferred = true;
                } else if !saw_transaction && token.eq_ignore_ascii_case(b"TRANSACTION") {
                    saw_transaction = true;
                } else {
                    return Some(CachedReadTransactionControl::Unsupported(keyword));
                }
                rest = next;
            }
            if !skip_sqlite_empty_prefix(rest).is_empty() {
                return Some(CachedReadTransactionControl::Unsupported(keyword));
            }
            Some(CachedReadTransactionControl::BeginDeferred)
        }
        "COMMIT" | "END" => Some(CachedReadTransactionControl::Finish(keyword)),
        "ROLLBACK" => {
            let first = next_sqlite_token(tail);
            let rollback_target = match first {
                Some((token, rest)) if token.eq_ignore_ascii_case(b"TRANSACTION") => {
                    next_sqlite_token(rest).map(|(token, _)| token)
                }
                Some((token, _)) => Some(token),
                None => None,
            };
            if rollback_target.is_some_and(|token| token.eq_ignore_ascii_case(b"TO")) {
                Some(CachedReadTransactionControl::Unsupported(keyword))
            } else {
                Some(CachedReadTransactionControl::Finish(keyword))
            }
        }
        _ => Some(CachedReadTransactionControl::Unsupported(keyword)),
    }
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

fn prepare_bound_statement<'conn>(
    conn: &'conn rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<rusqlite::Statement<'conn>, rusqlite::Error> {
    let mut stmt = prepare_sql_statement(conn, &statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;
    Ok(stmt)
}

fn execute_prepared_query(
    mut stmt: rusqlite::Statement<'_>,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
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

fn execute_prepared_query_row(
    mut stmt: rusqlite::Statement<'_>,
) -> Result<Option<SqlRow>, rusqlite::Error> {
    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();

    let mut raw_rows = stmt.raw_query();
    Ok(raw_rows
        .next()?
        .map(|row| row_to_sql_row(row, col_count, &col_names)))
}

fn execute_prepared_query_page(
    mut stmt: rusqlite::Statement<'_>,
    page: &PageRequest,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    // A zero-limit page still prepares and binds the statement, so invalid
    // SQL fails identically across every limit; it skips the row cursor
    // entirely and returns no rows.
    if page.limit == 0 {
        return Ok(Vec::new());
    }

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

/// Execute a query on a `rusqlite::Connection` and return owned rows.
fn execute_query(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    execute_prepared_query(prepare_bound_statement(conn, statement)?)
}

fn execute_query_row(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Option<SqlRow>, rusqlite::Error> {
    execute_prepared_query_row(prepare_bound_statement(conn, statement)?)
}

fn execute_query_page(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    page: &PageRequest,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    execute_prepared_query_page(prepare_bound_statement(conn, statement)?, page)
}

/// SQLite's prepared-statement classifier is authoritative for the safety
/// boundary. Row-producing DML (`UPDATE ... RETURNING`) and transaction
/// control may be called through `SqlReader`, but they are admitted writes and
/// must never register an interrupt target.
fn statement_is_cancellable_read(stmt: &rusqlite::Statement<'_>, sql: &str) -> bool {
    stmt.readonly() && transaction_control_head(sql).is_none()
}

/// Introspection `PRAGMA`s admitted through the pooled reader capability with
/// an optional single positional argument (`PRAGMA name` or `PRAGMA
/// name(arg)`) — none of these has a set/assignment form in SQLite, so an
/// argument is always a read-side filter, never a mutation.
const READER_STRUCTURAL_PRAGMAS: [&str; 8] = [
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "integrity_check",
];

/// Introspection `PRAGMA`s admitted through the pooled reader capability only
/// in their bare, argument-less read form. Every one of these also has a
/// `PRAGMA name = value` or `PRAGMA name(value)` assignment form in SQLite —
/// an assigning form changes connection-local state that a pristine-state
/// scan run only on dirty checkouts must never let back into the pool
/// unnoticed (`reader_connection_state_is_pristine`,
/// `reader_connection_settings_match_baseline`).
const READER_SETTING_PRAGMAS: [&str; 10] = [
    "database_list",
    "collation_list",
    "function_list",
    "compile_options",
    "page_count",
    "freelist_count",
    "user_version",
    "schema_version",
    "journal_mode",
    "page_size",
];

/// Reject any raw SQL that is not one of a small allow-listed set of
/// read-only statement shapes before it ever reaches a pooled reader
/// connection.
///
/// `stmt.readonly()` (SQLite's own classifier, used by
/// [`statement_is_cancellable_read`] to decide interrupt eligibility) is not
/// a sufficient admission predicate on its own: by SQLite's own definition it
/// also returns `true` for `ATTACH`/`DETACH`, `CREATE TEMP TABLE`, and
/// configuration `PRAGMA`s like `writable_schema`, `busy_timeout`, or
/// `cache_size` — none of which write the main database file, but all of
/// which leave connection-local state that persists across pooled checkouts.
/// Classification here instead runs on the statement head, admitting only
/// `SELECT`, `WITH ... SELECT`, `VALUES`, `EXPLAIN [QUERY PLAN] <admitted>`,
/// and the fixed `PRAGMA` allow-lists above.
fn reader_capability_admits(sql: &str) -> Result<(), String> {
    let rest = skip_sqlite_empty_prefix(sql.as_bytes());
    let Some((head, tail)) = next_sqlite_token(rest) else {
        // No head token (empty/comment-only statement): let SQLite's own
        // prepare step surface that error rather than duplicating it here.
        return Ok(());
    };
    if head.eq_ignore_ascii_case(b"SELECT")
        || head.eq_ignore_ascii_case(b"VALUES")
        || head.eq_ignore_ascii_case(b"WITH")
    {
        return Ok(());
    }
    if head.eq_ignore_ascii_case(b"EXPLAIN") {
        let mut rest = skip_sqlite_empty_prefix(tail);
        if let Some((query, next)) = next_sqlite_token(rest) {
            if query.eq_ignore_ascii_case(b"QUERY") {
                let after_query = skip_sqlite_empty_prefix(next);
                match next_sqlite_token(after_query) {
                    Some((plan, next2)) if plan.eq_ignore_ascii_case(b"PLAN") => {
                        rest = skip_sqlite_empty_prefix(next2);
                    }
                    _ => {
                        return Err(
                            "EXPLAIN QUERY must be followed by PLAN through the reader capability"
                                .into(),
                        );
                    }
                }
            }
        }
        return reader_capability_admits(&String::from_utf8_lossy(rest));
    }
    if head.eq_ignore_ascii_case(b"PRAGMA") {
        return reader_capability_admits_pragma(tail);
    }
    Err(format!(
        "statement head {:?} is not admitted through the reader capability; only \
         SELECT/WITH/VALUES/EXPLAIN and an allow-listed set of read-only PRAGMA forms \
         may run against a pooled reader connection",
        String::from_utf8_lossy(head)
    ))
}

fn reader_capability_admits_pragma(tail: &[u8]) -> Result<(), String> {
    let rest = skip_sqlite_empty_prefix(tail);
    let Some((mut name, mut after_name)) = next_sqlite_token(rest) else {
        return Err("PRAGMA with no name is not admitted through the reader capability".into());
    };
    // `schema.pragma_name` — the schema qualifier changes which attached
    // database the pragma targets, not which pragma runs.
    if after_name.first() == Some(&b'.') {
        let (qualified_name, qualified_after) =
            next_sqlite_token(&after_name[1..]).ok_or_else(|| {
                "PRAGMA schema-qualifier with no pragma name is not admitted through the \
                 reader capability"
                    .to_string()
            })?;
        name = qualified_name;
        after_name = qualified_after;
    }
    let after = skip_sqlite_empty_prefix(after_name);
    let is_structural = READER_STRUCTURAL_PRAGMAS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed.as_bytes()));
    let is_setting = READER_SETTING_PRAGMAS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed.as_bytes()));
    if !is_structural && !is_setting {
        return Err(format!(
            "PRAGMA {:?} is not admitted through the reader capability",
            String::from_utf8_lossy(name)
        ));
    }
    if after.first() == Some(&b'=') {
        return Err(format!(
            "PRAGMA {:?} may not be assigned through the reader capability",
            String::from_utf8_lossy(name)
        ));
    }
    if after.first() == Some(&b'(') && !is_structural {
        return Err(format!(
            "PRAGMA {:?} may not carry an argument through the reader capability",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

/// Refuse a statement bound for the reader capability that is neither
/// admitted transaction control (handled by the caller's cached
/// read-transaction state machine) nor an admitted read shape
/// ([`reader_capability_admits`]).
fn admit_reader_capability_sql(
    statement: &SqlStatement,
    transaction_control: Option<CachedReadTransactionControl>,
    operation: &'static str,
) -> khive_storage::types::StorageResult<()> {
    // Only the admitted single-level deferred-read span (`BEGIN`/`BEGIN
    // DEFERRED` and its `COMMIT`/`END`/`ROLLBACK` counterpart) bypasses the
    // read-shape check below; `Unsupported` forms (`BEGIN IMMEDIATE`,
    // `SAVEPOINT`, `ROLLBACK TO ...`) fall through and are refused there like
    // any other non-admitted statement head.
    if matches!(
        transaction_control,
        Some(CachedReadTransactionControl::BeginDeferred)
            | Some(CachedReadTransactionControl::Finish(_))
    ) {
        return Ok(());
    }
    reader_capability_admits(&statement.sql).map_err(|message| StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: operation.into(),
        message,
    })
}

fn execute_query_interruptibly(
    scope: &crate::read_cancellation::InterruptibleReadScope,
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    operation: &'static str,
    rollback_interrupted_transaction: bool,
    interruptible: bool,
) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
    let stmt = prepare_bound_statement(conn, statement)
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if interruptible && statement_is_cancellable_read(&stmt, &statement.sql) {
        scope.run_with_interrupted_cleanup(
            conn,
            move || {
                execute_prepared_query(stmt).map_err(|error| map_rusqlite_err(error, operation))
            },
            || {
                rollback_interrupted_read_transaction(
                    conn,
                    operation,
                    rollback_interrupted_transaction,
                )
            },
        )
    } else {
        scope.mark_write_committed()?;
        execute_prepared_query(stmt).map_err(|error| map_rusqlite_err(error, operation))
    }
}

fn execute_query_row_interruptibly(
    scope: &crate::read_cancellation::InterruptibleReadScope,
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    operation: &'static str,
    rollback_interrupted_transaction: bool,
    interruptible: bool,
) -> khive_storage::types::StorageResult<Option<SqlRow>> {
    let stmt = prepare_bound_statement(conn, statement)
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if interruptible && statement_is_cancellable_read(&stmt, &statement.sql) {
        scope.run_with_interrupted_cleanup(
            conn,
            move || {
                execute_prepared_query_row(stmt).map_err(|error| map_rusqlite_err(error, operation))
            },
            || {
                rollback_interrupted_read_transaction(
                    conn,
                    operation,
                    rollback_interrupted_transaction,
                )
            },
        )
    } else {
        scope.mark_write_committed()?;
        execute_prepared_query_row(stmt).map_err(|error| map_rusqlite_err(error, operation))
    }
}

fn execute_query_page_interruptibly(
    scope: &crate::read_cancellation::InterruptibleReadScope,
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    page: &PageRequest,
    operation: &'static str,
    rollback_interrupted_transaction: bool,
    interruptible: bool,
) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
    let stmt = prepare_bound_statement(conn, statement)
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if interruptible && statement_is_cancellable_read(&stmt, &statement.sql) {
        scope.run_with_interrupted_cleanup(
            conn,
            move || {
                execute_prepared_query_page(stmt, page)
                    .map_err(|error| map_rusqlite_err(error, operation))
            },
            || {
                rollback_interrupted_read_transaction(
                    conn,
                    operation,
                    rollback_interrupted_transaction,
                )
            },
        )
    } else {
        scope.mark_write_committed()?;
        execute_prepared_query_page(stmt, page).map_err(|error| map_rusqlite_err(error, operation))
    }
}

fn rollback_interrupted_read_transaction(
    conn: &rusqlite::Connection,
    operation: &'static str,
    enabled: bool,
) -> khive_storage::types::StorageResult<()> {
    if !enabled || conn.is_autocommit() {
        return Ok(());
    }
    conn.execute_batch("ROLLBACK")
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if conn.is_autocommit() {
        Ok(())
    } else {
        Err(StorageError::Transaction {
            operation: operation.into(),
            message: "interrupted read transaction rollback did not restore autocommit".into(),
        })
    }
}

/// Map a rusqlite error to `StorageError`.
fn map_rusqlite_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Sql, op, e)
}

/// How an elapsed handle-slot deadline is classified. ADR-005 pins the closed
/// raw-SQL standalone exception (`sql_bridge.reader_open`) and reads on a
/// standalone writer (`sql_bridge.reader_operation`) to
/// `StorageError::Timeout`; ordinary reader-pool saturation reports the typed
/// `AdmissionTimeout` through `ConnectionPool::resolve_reader_checkout`.
#[derive(Clone, Copy)]
enum SlotTimeoutClass {
    Admission,
    ReaderContract,
}

async fn acquire_reader_handle_slot(
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

async fn acquire_handle_slot(
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

fn open_standalone_reader(pool: &ConnectionPool) -> Result<rusqlite::Connection, StorageError> {
    pool.open_standalone_reader(StandaloneReaderPurpose::ExplicitSqlReadTransaction)
        .map_err(|error| StorageError::driver(StorageCapability::Sql, "open_reader", error))
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
// File-backed: pooled SqliteReader with an explicit transaction exception
// =============================================================================

const CACHED_READ_TRANSACTION_LABEL: &str = "sql_bridge_cached_read_transaction";

/// Admission and observability guards for one explicit cached-reader
/// transaction. Both guards are installed only after SQLite accepts `BEGIN`
/// and are retained together until SQLite reports autocommit again or the
/// owning connection is closed.
///
/// Field order is deliberate: after [`StandaloneHandle::conn`] closes, the
/// reader permit is returned before the registry evidence disappears. There
/// is therefore no interval in which SQLite can still own the snapshot while
/// the transaction is absent from `tx_registry`.
struct CachedReadTransaction {
    _slot: OwnedSemaphorePermit,
    _tx_handle: khive_storage::tx_registry::TxHandle,
    /// When this explicit `BEGIN` was admitted. Read on every subsequent
    /// reuse of the owning cached-reader handle (#1846): a transaction whose
    /// age has crossed `read_tx_max_age` is rolled back instead of being
    /// extended by another call, bounding how long any one reader can pin
    /// the WAL snapshot regardless of how many further requests it makes.
    opened_at: Instant,
}

struct StandaloneHandle {
    conn: rusqlite::Connection,
    /// Present only for a standalone read-write handle, whose one-permit
    /// connection budget remains handle-scoped. Read-only connections exist
    /// only for explicit deferred transactions and retain reader admission in
    /// `read_transaction_slot` until terminal control.
    _retained_slot: Option<OwnedSemaphorePermit>,
    /// Present only while a cached read-only connection owns one explicit
    /// multi-call read transaction. Field order is load-bearing: Rust drops
    /// `conn` before these guards, so cancellation or handle drop closes the
    /// SQLite transaction before returning reader admission or deregistering
    /// the transaction span.
    read_transaction_slot: Option<CachedReadTransaction>,
}

impl StandaloneHandle {
    /// Whether this is an idle-cacheable read-only connection rather than a
    /// writer connection covered by the handle-scoped writer permit.
    fn is_cached_reader(&self) -> bool {
        self._retained_slot.is_none()
    }

    fn has_read_transaction(&self) -> bool {
        self.read_transaction_slot.is_some()
    }
}

struct SqliteReader {
    /// Present only for the ADR-005/ADR-091 explicitly admitted multi-call
    /// deferred transaction. Ordinary reads never populate this field and
    /// route through `ConnectionPool::reader` for each operation.
    handle: Option<StandaloneHandle>,
    pool: Arc<ConnectionPool>,
    /// Fail-loud compatibility state after a transaction connection could not
    /// be restored safely. `None` normally means "pooled ordinary route";
    /// this bit distinguishes that from "exceptional connection consumed".
    poisoned: bool,
}

async fn open_explicit_read_transaction_handle(
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
async fn execute_standalone_read<R, F>(
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
        Some(
            acquire_reader_handle_slot(
                &pool,
                "sql_bridge.reader_operation",
                SlotTimeoutClass::ReaderContract,
            )
            .await?,
        )
    } else {
        Some(
            crate::await_request_read_phase(
                "sql_bridge.reader_operation",
                acquire_reader_handle_slot(
                    &pool,
                    "sql_bridge.reader_operation",
                    SlotTimeoutClass::ReaderContract,
                ),
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
                "sql_bridge.reader_operation",
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
                "sql_bridge.reader_operation",
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
                "sql_bridge.reader_operation",
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

// =============================================================================
// File-backed: SqliteWriter (standalone connection)
// =============================================================================

struct SqliteWriter {
    /// `None` at construction when a `WriterTaskHandle` was obtained (ADR-136
    /// D1 gate 1: queue-first `writer()` skips the standalone open in that
    /// case). Ordinary `SqlReader` supertrait calls then use pooled readers;
    /// this slot is populated only while such a queue-backed handle owns an
    /// explicit deferred read transaction. An eagerly opened read-write
    /// connection (the no-writer-task branch) retains its one-permit writer
    /// budget for the handle's lifetime and must serve its reads on that same
    /// connection to preserve manual-transaction visibility.
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
    /// Reader-pool owner and explicit-transaction exception source.
    pool: Arc<ConnectionPool>,
}

impl SqliteWriter {
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
                return run_pool_reader_query(
                    Arc::clone(&self.pool),
                    "sql_bridge.reader_operation",
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
        let transaction_control = cached_read_transaction_control(&statement.sql);
        execute_standalone_read(
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
                return run_pool_reader_query(
                    Arc::clone(&self.pool),
                    "sql_bridge.reader_operation",
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
        let transaction_control = cached_read_transaction_control(&statement.sql);
        execute_standalone_read(
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
                return run_pool_reader_query(
                    Arc::clone(&self.pool),
                    "sql_bridge.reader_operation",
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
        let transaction_control = cached_read_transaction_control(&statement.sql);
        execute_standalone_read(
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

async fn run_pool_reader_query<T, F>(
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
                pool.reader_until(|| scope.should_stop()),
            )?;
            // Every caller of `run_pool_reader_query` runs a `SqlStatement`
            // (raw SQL) rather than a typed store's fixed query, so the
            // checkout pays the pristine-state scan on return regardless of
            // which `SqlReader` wrapper (reader or writer capability) drew it.
            guard.mark_dirty();
            scope.with_pooled_reader(&mut guard, |conn| query(scope, conn))
        },
    )
    .await
}

async fn run_pool_writer_query<T, F>(
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
        ) -> khive_storage::types::StorageResult<T>
        + Send
        + 'static,
{
    crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        operation,
        move |scope| {
            let guard = pool.try_writer().map_err(|error: SqliteError| {
                StorageError::driver(StorageCapability::Sql, operation, error)
            })?;
            scope.with_pooled_writer(&pool, &guard, |conn| {
                let interruptible = conn.is_autocommit();
                query(scope, conn, interruptible)
            })
        },
    )
    .await
}

struct PoolBackedReader {
    pool: Arc<ConnectionPool>,
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
        run_pool_reader_query(pool, "pool_reader.query_row", move |scope, conn| {
            execute_query_row_interruptibly(
                scope,
                conn,
                &statement,
                "pool_reader.query_row",
                false,
                true,
            )
        })
        .await
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let transaction_control = cached_read_transaction_control(&statement.sql);
        admit_reader_capability_sql(&statement, transaction_control, "pool_reader.query_all")?;
        let pool = Arc::clone(&self.pool);
        run_pool_reader_query(pool, "pool_reader.query_all", move |scope, conn| {
            execute_query_interruptibly(
                scope,
                conn,
                &statement,
                "pool_reader.query_all",
                false,
                true,
            )
        })
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
        run_pool_reader_query(pool, "pool_reader.query_page", move |scope, conn| {
            execute_query_page_interruptibly(
                scope,
                conn,
                &statement,
                &page,
                "pool_reader.query_page",
                false,
                true,
            )
        })
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
        run_pool_writer_query(
            pool,
            "pool_writer.query_row",
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
                        let _ = guard.execute_batch("ROLLBACK");
                        Err(map_rusqlite_err(e, "pool_writer.execute_batch"))
                    } else {
                        Ok(total)
                    }
                }
                Err(e) => {
                    let _ = guard.execute_batch("ROLLBACK");
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
/// - File-backed: pooled ordinary reader operations and a lazy standalone
///   connection only for explicitly admitted multi-call read transactions,
///   plus standalone writer connections capped at one live handle; atomic
///   units drive a single registered raw transaction span.
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
            let handle = if writer_task.is_none() {
                let handle_slot = acquire_handle_slot(
                    self.pool.sql_bridge_writer_slots(),
                    self.pool.config().checkout_timeout,
                    "sql_bridge.writer_handle",
                    SlotTimeoutClass::Admission,
                )
                .await?;
                let (conn, handle_slot) =
                    open_standalone_writer_on_blocking(Arc::clone(&self.pool), handle_slot).await?;
                Some(StandaloneHandle {
                    conn,
                    _retained_slot: Some(handle_slot),
                    read_transaction_slot: None,
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
            let (conn, handle_slot) =
                open_standalone_writer_on_blocking(Arc::clone(&self.pool), handle_slot).await?;
            let mut writer = SqliteWriter {
                handle: Some(StandaloneHandle {
                    conn,
                    _retained_slot: Some(handle_slot),
                    read_transaction_slot: None,
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

    fn database_tx_view(pool: &ConnectionPool) -> khive_storage::tx_registry::TxOriginFilter {
        match pool.origin() {
            khive_storage::tx_registry::TxOrigin::Database(identity) => {
                khive_storage::tx_registry::TxOriginFilter::Secondary(identity)
            }
            other => panic!("expected a file-backed database origin, got {other:?}"),
        }
    }

    struct NotifyOnDrop(Arc<tokio::sync::Notify>);

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    fn blocking_non_interrupting_progress_gate(
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
            1_000,
            Some(move || {
                let _keep_until_connection_drop = &notify_on_drop;
                if !callback_blocked_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    callback_entered.notify_one();
                    callback_release.wait();
                    // The gate deliberately stalls but never asks SQLite to
                    // abort. It proves completion-preserving SQLite work ignores
                    // request-read cancellation and finishes normally.
                    return false;
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

    fn slow_insert_statement() -> SqlStatement {
        SqlStatement {
            sql: "INSERT INTO cancellation_write_probe(value) \
                  WITH RECURSIVE rows(value) AS (\
                  SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 10000\
                  ) SELECT value FROM rows"
                .into(),
            params: vec![],
            label: Some("non-interruptible-write-probe".into()),
        }
    }

    fn passive_checkpoint(conn: &rusqlite::Connection) -> (i64, i64, i64) {
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
    }

    fn deliberately_slow_read_statement() -> SqlStatement {
        SqlStatement {
            sql: "WITH RECURSIVE numbers(value) AS (\
                  SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
                  ) SELECT SUM(a.value * b.value * c.value) \
                  FROM numbers AS a CROSS JOIN numbers AS b CROSS JOIN numbers AS c"
                .into(),
            params: vec![],
            label: Some("read-cancellation-progress-probe".into()),
        }
    }

    async fn wait_for_progress(probe: &std::sync::atomic::AtomicUsize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while probe.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("slow SQLite statement never reached its progress callback");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_before_reader_checkout_is_prompt_and_executes_no_statement() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancel_before_checkout.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_secs(5),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE checkout_cancel_probe(value INTEGER NOT NULL); \
                 INSERT INTO checkout_cancel_probe VALUES (0);",
            )
            .unwrap();
        let held_reader = pool.reader().expect("hold the sole pooled reader");
        // Production-shaped file-backed raw-SQL reads must use the same pool
        // admission as typed stores. Before ADR-165 Slice 2, `reader()` opened
        // a standalone connection and this held pooled reader was invisible.
        // A plain SELECT (not a DML/RETURNING statement) is used here: the
        // reader capability's admission gate now refuses non-read statement
        // shapes before checkout is even attempted, so a DML probe would
        // never reach the cancellation-race path this test exercises.
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut waiting_reader = bridge.reader().await.unwrap();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let waiting = tokio::spawn(crate::scope_request_read_cancellation(
            cancel_rx,
            async move {
                waiting_reader
                    .query_row(SqlStatement {
                        sql: "SELECT value FROM checkout_cancel_probe".into(),
                        params: vec![],
                        label: Some("must-not-run-after-cancelled-checkout".into()),
                    })
                    .await
            },
        ));

        tokio::task::yield_now().await;
        cancel_tx.send(true).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
            .await
            .expect("cancelled reader checkout waited for the five-second pool timeout")
            .expect("checkout task panicked");
        // Cancellation is NOT an admission wait: it must stay the non-admission
        // Timeout, never the retryable AdmissionTimeout, so a cancelled request
        // does not signal clients to retry into a saturated pool.
        assert!(matches!(result, Err(StorageError::Timeout { .. })));

        drop(held_reader);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let value: i64 = pool
            .reader()
            .unwrap()
            .conn()
            .query_row("SELECT value FROM checkout_cancel_probe", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            value, 0,
            "the probe row must be untouched: nothing else in this test writes to it"
        );
        assert_eq!(
            pool.available_readers(),
            1,
            "reader checkout leaked a permit"
        );
    }

    /// Reproduction probe for the reader-capability admission gap: `sqlite3_stmt_readonly`
    /// (the only check the pre-fix bridge applied to raw SQL reaching the
    /// pooled reader) returns `true` for `ATTACH`, configuration `PRAGMA`s,
    /// and `CREATE TEMP TABLE` — none of which write the main database file,
    /// but all of which leave connection-local state that `reset_reader_connection`
    /// did not catch. Each probe below is submitted through the reader
    /// capability exactly as an untrusted caller would reach it
    /// (`SqlBridge::reader` -> `SqlReader::query_all`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_reader_capability_admission_of_state_mutating_statements() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_admission_probe.db")),
            max_readers: 2,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch("CREATE TABLE reader_admission_probe(value INTEGER NOT NULL);")
            .unwrap();
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let probes: [&str; 5] = [
            "ATTACH DATABASE ':memory:' AS x",
            "PRAGMA writable_schema=ON",
            "PRAGMA busy_timeout=1",
            "PRAGMA cache_size(-64)",
            "CREATE TEMP TABLE t(x)",
        ];
        let mut admitted = Vec::new();
        for probe in probes {
            let mut reader = bridge.reader().await.unwrap();
            let result = reader
                .query_all(SqlStatement {
                    sql: probe.into(),
                    params: vec![],
                    label: Some("reader-admission-probe".into()),
                })
                .await;
            admitted.push((probe, result.is_ok()));
        }
        eprintln!("reader capability admission per probe: {admitted:#?}");
        for (probe, was_admitted) in &admitted {
            assert!(
                !was_admitted,
                "reader capability must refuse {probe:?}; the pre-fix bridge wrongly admitted it"
            );
        }

        // Control: an ordinary SELECT and an allow-listed introspection
        // PRAGMA must still be admitted through the same reader capability.
        let controls: [&str; 2] = [
            "SELECT value FROM reader_admission_probe",
            "PRAGMA table_info(reader_admission_probe)",
        ];
        for control in controls {
            let mut reader = bridge.reader().await.unwrap();
            let result = reader
                .query_all(SqlStatement {
                    sql: control.into(),
                    params: vec![],
                    label: Some("reader-admission-control".into()),
                })
                .await;
            assert!(
                result.is_ok(),
                "reader capability must still admit {control:?}: {result:?}"
            );
        }
    }

    /// Regression for the in-memory reader path: `PoolBackedReader` (the
    /// `SqlBridge::reader` implementation used whenever `is_file_backed` is
    /// `false`) computed its admission classification differently from
    /// `SqliteReader`, always passing `None` for the cached transaction-control
    /// read instead of classifying the statement. That made the deferred-read
    /// snapshot pair (`BEGIN DEFERRED` ... `COMMIT`) a multi-call reader
    /// already relies on to hold one consistent view across several pooled
    /// checkouts — see `khive-pack-memory`'s fresh-tail leg — indistinguishable
    /// from an unrecognized statement head and refused outright. Mutating
    /// statements must still be refused through this path exactly as they are
    /// through the file-backed one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_backed_reader_admits_deferred_read_transaction_control() {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE pool_backed_reader_probe(value INTEGER NOT NULL); \
                 INSERT INTO pool_backed_reader_probe VALUES (1);",
            )
            .unwrap();
        let bridge = SqlBridge::new(Arc::clone(&pool), false);
        let mut reader = bridge.reader().await.unwrap();

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: Some("pool-backed-reader-snapshot-begin".into()),
            })
            .await
            .expect("BEGIN DEFERRED must be admitted through the pool-backed reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT value FROM pool_backed_reader_probe".into(),
                params: vec![],
                label: Some("pool-backed-reader-snapshot-read".into()),
            })
            .await
            .expect("a read inside the admitted snapshot must succeed");
        assert_eq!(rows.len(), 1);
        reader
            .query_all(SqlStatement {
                sql: "COMMIT".into(),
                params: vec![],
                label: Some("pool-backed-reader-snapshot-commit".into()),
            })
            .await
            .expect("COMMIT must be admitted through the pool-backed reader");

        let integrity = reader
            .query_scalar(SqlStatement {
                sql: "PRAGMA integrity_check".into(),
                params: vec![],
                label: Some("pool-backed-reader-integrity-check".into()),
            })
            .await
            .expect("PRAGMA integrity_check must be admitted through the pool-backed reader");
        assert!(matches!(integrity, Some(SqlValue::Text(ref s)) if s.eq_ignore_ascii_case("ok")));

        for probe in [
            "ATTACH DATABASE ':memory:' AS x",
            "PRAGMA writable_schema=ON",
            "CREATE TEMP TABLE t(x)",
            "SAVEPOINT nested_snapshot",
        ] {
            let mut reader = bridge.reader().await.unwrap();
            let result = reader
                .query_all(SqlStatement {
                    sql: probe.into(),
                    params: vec![],
                    label: Some("pool-backed-reader-admission-probe".into()),
                })
                .await;
            assert!(
                result.is_err(),
                "pool-backed reader capability must refuse {probe:?}; got {result:?}"
            );
        }
    }

    /// A pooled-reader checkout that exhausts `checkout_timeout` WITHOUT any
    /// cancellation is a genuine admission wait and must surface as the
    /// retryable AdmissionTimeout. Before the fix, `reader_until`'s
    /// pool-exhausted error was mapped to `StorageError::Driver`, so a
    /// saturated pooled read stayed a non-retryable driver failure and the new
    /// AdmissionTimeout branch (reachable only for `Ok(None)` cancellation) was
    /// dead for real timeouts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pooled_reader_checkout_timeout_is_a_retryable_admission_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_admission_timeout.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(200),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE reader_admission_probe(value INTEGER NOT NULL); \
                 INSERT INTO reader_admission_probe VALUES (0);",
            )
            .unwrap();
        // Hold the sole pooled reader so the contending checkout cannot succeed
        // and must run `checkout_timeout` to exhaustion — no cancellation.
        let held_reader = pool.reader().expect("hold the sole pooled reader");

        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut contender = bridge.reader().await.unwrap();
        let blocked = contender
            .query_row(SqlStatement {
                sql: "SELECT value FROM reader_admission_probe".into(),
                params: vec![],
                label: Some("reader-admission-timeout-probe".into()),
            })
            .await;
        assert!(
            matches!(blocked, Err(StorageError::AdmissionTimeout { .. })),
            "an exhausted pooled-reader checkout must be a retryable AdmissionTimeout; got {blocked:?}"
        );

        drop(held_reader);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(
            pool.available_readers(),
            1,
            "reader checkout leaked a permit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_read_interrupts_sqlite_releases_permit_and_stops_work() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_abandoned_read.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(500),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = SqliteReader {
            handle: Some(
                open_explicit_read_transaction_handle(Arc::clone(&pool))
                    .await
                    .unwrap(),
            ),
            pool: Arc::clone(&pool),
            poisoned: false,
        };
        let mut contender = bridge.reader().await.unwrap();
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let progress_in_scope = Arc::clone(&progress);

        let query = tokio::spawn(crate::scope_test_read_progress(
            progress_in_scope,
            async move { reader.query_all(deliberately_slow_read_statement()).await },
        ));
        wait_for_progress(progress.as_ref()).await;
        query.abort();
        assert!(matches!(query.await, Err(error) if error.is_cancelled()));

        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            contender.query_row(SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            }),
        )
        .await
        .expect("abandoned SQLite statement did not return the sole reader promptly")
        .expect("reader probe failed after cancellation");

        let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            progress.load(std::sync::atomic::Ordering::SeqCst),
            stopped_at,
            "SQLite progress kept advancing after the abandoned request returned its reader"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_deadline_interrupts_statement_without_outer_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_request_deadline.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(500),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let result = crate::scope_test_read_progress(
            Arc::clone(&progress),
            crate::scope_request_read_deadline(std::time::Duration::from_millis(25), async move {
                reader.query_all(deliberately_slow_read_statement()).await
            }),
        )
        .await;
        assert!(
            matches!(result, Err(StorageError::Timeout { .. })),
            "deadline must surface as a typed timeout, got {result:?}"
        );

        let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
        assert!(stopped_at > 0, "deadline test never exercised SQLite work");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            progress.load(std::sync::atomic::Ordering::SeqCst),
            stopped_at,
            "deadline returned while SQLite kept consuming work"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_handler_cleanup_failure_discards_pooled_connection() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cleanup_failure.db")),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let pool_for_read = Arc::clone(&pool);
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = crate::read_cancellation::scope_test_read_cleanup_failure(
            crate::scope_test_read_progress(
                Arc::clone(&progress),
                crate::read_cancellation::run_interruptible_read(
                    StorageCapability::Sql,
                    "cleanup_failure_probe",
                    move |scope| {
                        let mut guard = pool_for_read.reader().map_err(|error| {
                            StorageError::driver(
                                StorageCapability::Sql,
                                "cleanup_failure_probe",
                                error,
                            )
                        })?;
                        scope.run_pooled_reader(&mut guard, |conn| {
                            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                                .map_err(|error| map_rusqlite_err(error, "cleanup_failure_probe"))
                        })
                    },
                ),
            ),
        )
        .await;
        assert!(
            matches!(result, Err(StorageError::Internal(ref message)) if message.contains("clear failure")),
            "injected cleanup failure must be surfaced; got {result:?}"
        );
        assert_eq!(
            pool.available_readers(),
            1,
            "discard must install a replacement"
        );

        let calls_after_failed_read = progress.load(std::sync::atomic::Ordering::SeqCst);
        let guard = pool.reader().unwrap();
        let sum: i64 = guard
            .conn()
            .query_row(
                "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < 10000) \
                 SELECT sum(x) FROM n",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sum, 50_005_000);
        assert_eq!(
            progress.load(std::sync::atomic::Ordering::SeqCst),
            calls_after_failed_read,
            "a connection whose handler could not be cleared was reused"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_pooled_reader_quarantines_cleanup_failure_during_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("raw_reader_unwind_cleanup.db")),
                max_readers: 1,
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        let worker_pool = Arc::clone(&pool);
        let result = crate::read_cancellation::scope_test_read_cleanup_failure(
            crate::read_cancellation::run_interruptible_read(
                StorageCapability::Sql,
                "raw_reader_unwind_cleanup",
                move |scope| {
                    let mut guard = worker_pool.reader().map_err(|error| {
                        StorageError::driver(
                            StorageCapability::Sql,
                            "raw_reader_unwind_cleanup",
                            error,
                        )
                    })?;
                    scope.with_pooled_reader(&mut guard, |conn| {
                        scope.run(conn, || -> khive_storage::types::StorageResult<()> {
                            panic!("injected raw reader panic after progress registration")
                        })
                    })
                },
            ),
        )
        .await;
        assert!(
            result.is_err(),
            "blocking panic must surface as a join error"
        );
        assert_eq!(
            pool.available_readers(),
            pool.max_readers(),
            "unwind cleanup failure must close and replace the raw pooled reader"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_pooled_writer_retires_cleanup_failure_during_unwind() {
        let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
        let worker_pool = Arc::clone(&pool);
        let result = crate::read_cancellation::scope_test_read_cleanup_failure(
            crate::read_cancellation::run_interruptible_read(
                StorageCapability::Sql,
                "raw_writer_unwind_cleanup",
                move |scope| {
                    let guard = worker_pool.try_writer().map_err(|error| {
                        StorageError::driver(
                            StorageCapability::Sql,
                            "raw_writer_unwind_cleanup",
                            error,
                        )
                    })?;
                    scope.with_pooled_writer(&worker_pool, &guard, |conn| {
                        scope.run(conn, || -> khive_storage::types::StorageResult<()> {
                            panic!("injected raw writer panic after progress registration")
                        })
                    })
                },
            ),
        )
        .await;
        assert!(
            result.is_err(),
            "blocking panic must surface as a join error"
        );
        assert!(
            pool.try_writer().is_err(),
            "unwind cleanup failure must retire the raw pooled writer"
        );
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
    async fn file_bridge_scopes_reader_permits_to_operations_and_caps_writer_handles() {
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

        let mut retained_readers = Vec::new();
        for expected in 0..3 {
            let mut reader = second_bridge.reader().await.unwrap();
            let value = reader
                .query_scalar(SqlStatement {
                    sql: format!("SELECT {expected}"),
                    params: vec![],
                    label: None,
                })
                .await
                .unwrap();
            assert!(matches!(value, Some(SqlValue::Integer(value)) if value == expected));
            retained_readers.push(reader);
        }
        assert_eq!(retained_readers.len(), 3);

        let mut additional_reader = bridge.reader().await.unwrap();
        let page = additional_reader
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
        drop((additional_reader, retained_readers));

        let writer = bridge.writer().await.unwrap();
        let writer_error = match second_bridge.writer().await {
            Ok(_) => panic!("a second live writer handle exceeded the one-handle cap"),
            Err(error) => error,
        };
        assert!(matches!(
            writer_error,
            StorageError::AdmissionTimeout { ref operation, .. }
                if operation.as_ref() == "sql_bridge.writer_handle"
        ));
        drop(writer);
        let writer_after_release = bridge.writer().await.unwrap();
        drop(writer_after_release);
    }

    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn file_bridge_attributes_ordinary_pool_reads_and_explicit_transaction_exception() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("sql_bridge_reader_routes.db")),
                max_readers: 1,
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();

        assert_eq!(
            pool.reader_acquisition_snapshot(),
            crate::pool::ReaderAcquisitionSnapshot {
                reader_admission_capacity: 1,
                available_reader_admission_slots: 1,
                ..crate::pool::ReaderAcquisitionSnapshot::default()
            },
            "constructing or retaining an idle raw-SQL reader must open nothing"
        );

        let value = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(matches!(value, Some(SqlValue::Integer(1))));
        let ordinary = pool.reader_acquisition_snapshot();
        assert_eq!(ordinary.pooled_checkouts, 1);
        assert_eq!(ordinary.completed_pooled_checkouts, 1);
        assert_eq!(ordinary.standalone_opens, 0);

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("the documented explicit transaction exception opens");
        let begun = pool.reader_acquisition_snapshot();
        assert_eq!(begun.pooled_checkouts, 1);
        assert_eq!(begun.standalone_opens, 1);

        reader
            .query_scalar(SqlStatement {
                sql: "SELECT 2".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("transaction query reuses its one exceptional connection");
        reader
            .query_all(SqlStatement {
                sql: "COMMIT".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("COMMIT closes the explicit transaction exception");
        let committed = pool.reader_acquisition_snapshot();
        assert_eq!(committed.reader_admission_capacity, 1);
        assert_eq!(committed.available_reader_admission_slots, 1);
        assert_eq!(committed.acquisitions, begun.acquisitions);
        assert_eq!(committed.pooled_checkouts, begun.pooled_checkouts);
        assert_eq!(committed.standalone_opens, begun.standalone_opens);
        assert_eq!(
            committed.completed_pooled_checkouts, begun.completed_pooled_checkouts,
            "queries and COMMIT inside one explicit transaction must not acquire another reader"
        );

        reader
            .query_scalar(SqlStatement {
                sql: "SELECT 3".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("ordinary traffic returns to the reader pool after COMMIT");
        let after = pool.reader_acquisition_snapshot();
        assert_eq!(after.pooled_checkouts, 2);
        assert_eq!(after.completed_pooled_checkouts, 2);
        assert_eq!(after.standalone_opens, 1);
        assert_eq!(after.active_pooled_checkouts, 0);
    }

    #[tokio::test]
    async fn explicit_reader_open_timeout_is_visible_without_a_standalone_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("sql_bridge_reader_open_timeout.db")),
                max_readers: 1,
                checkout_timeout: std::time::Duration::from_millis(20),
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut logical_reader = bridge.reader().await.unwrap();
        let held = pool.reader().expect("hold the shared reader budget");

        let blocked = logical_reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &blocked,
                Err(StorageError::Timeout { operation })
                    if operation.as_ref() == "sql_bridge.reader_open"
            ),
            "the compatible explicit-transaction open phase must stay visible; got {blocked:?}"
        );

        let snapshot = pool.reader_acquisition_snapshot();
        assert_eq!(snapshot.checkout_timeouts, 1);
        assert_eq!(snapshot.pooled_checkouts, 1);
        assert_eq!(snapshot.standalone_opens, 0);
        assert_eq!(snapshot.active_pooled_checkouts, 1);
        assert_eq!(snapshot.available_reader_admission_slots, 0);
        drop(held);
    }

    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn cached_read_transaction_retains_one_permit_until_commit_or_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_tx_control.db")),
            write_queue_enabled: Some(true),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(20),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin = pool.origin();
        let origin_view = database_tx_view(&pool);
        let unrelated_view = khive_storage::tx_registry::TxOriginFilter::Secondary(
            khive_storage::tx_registry::DbIdentity::new("unrelated-sql-bridge.db"),
        );
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();
        let mut contender = bridge.reader().await.unwrap();

        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "an idle cached reader must not register a transaction"
        );

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("BEGIN DEFERRED must open an admitted cached-reader snapshot");
        let opened = khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("successful BEGIN must register the cached-reader transaction");
        assert_eq!(opened.label.as_deref(), Some(CACHED_READ_TRANSACTION_LABEL));
        assert_eq!(opened.origin, origin);
        assert!(
            khive_storage::tx_registry::oldest_for(&unrelated_view).is_none(),
            "the read transaction must be attributed only to its own backend"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            0,
            "the successful BEGIN must retain its operation permit"
        );

        let value = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 7".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("a query inside the admitted transaction must reuse its retained permit");
        assert!(matches!(value, Some(SqlValue::Integer(7))));
        assert_eq!(
            khive_storage::tx_registry::oldest_for(&origin_view)
                .expect("queries must retain the transaction registration")
                .id,
            opened.id,
            "queries inside the transaction must retain the original span"
        );

        let blocked = contender
            .query_scalar(SqlStatement {
                sql: "SELECT 8".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &blocked,
                Err(StorageError::AdmissionTimeout { operation, .. })
                    if operation.as_ref() == "sql_bridge.reader_operation"
            ),
            "a second logical read must contend with the admitted transaction \
             and fail at the bounded pooled-admission stage; got {blocked:?}"
        );

        reader
            .query_all(SqlStatement {
                sql: "COMMIT".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("COMMIT must close the admitted cached-reader snapshot");
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "COMMIT must deregister after SQLite returns to autocommit"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "COMMIT may release the permit only after autocommit is restored"
        );
        let value = contender
            .query_scalar(SqlStatement {
                sql: "SELECT 8".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("the contender must run after COMMIT releases admission");
        assert!(matches!(value, Some(SqlValue::Integer(8))));

        reader
            .query_all(SqlStatement {
                sql: "BEGIN TRANSACTION".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("plain deferred BEGIN TRANSACTION must also be admitted");
        let reopened = khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("the second successful BEGIN must register a fresh span");
        assert_ne!(reopened.id, opened.id);
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
        let nested = reader
            .query_all(SqlStatement {
                sql: "ROLLBACK TO stale_snapshot".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(&nested, Err(StorageError::InvalidInput { .. })),
            "ROLLBACK TO requires unsupported nested state; got {nested:?}"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            0,
            "rejected nested control must not release the still-live transaction admission"
        );
        assert_eq!(
            khive_storage::tx_registry::oldest_for(&origin_view)
                .expect("ROLLBACK TO rejection must retain the live span")
                .id,
            reopened.id
        );
        let savepoint = reader
            .query_all(SqlStatement {
                sql: "SAVEPOINT nested_snapshot".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(&savepoint, Err(StorageError::InvalidInput { .. })),
            "SAVEPOINT must be rejected inside the admitted transaction; got {savepoint:?}"
        );
        assert_eq!(
            khive_storage::tx_registry::oldest_for(&origin_view)
                .expect("SAVEPOINT rejection must retain the live span")
                .id,
            reopened.id
        );
        reader
            .query_all(SqlStatement {
                sql: "ROLLBACK".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("ROLLBACK must close the admitted cached-reader snapshot");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "full ROLLBACK must deregister after SQLite returns to autocommit"
        );
    }

    #[tokio::test]
    async fn failed_cached_reader_begin_does_not_register_a_transaction() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

        fn deny_begin(ctx: AuthContext<'_>) -> Authorization {
            match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_failed_begin.db")),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin_view = database_tx_view(&pool);
        let conn = open_standalone_reader(&pool).unwrap();
        conn.authorizer(Some(deny_begin)).unwrap();
        let mut reader = SqliteReader {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: None,
                read_transaction_slot: None,
            }),
            pool: Arc::clone(&pool),
            poisoned: false,
        };

        let begin = reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(begin.is_err(), "the authorizer must reject BEGIN");
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "a failed BEGIN must never enter the transaction registry"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "a failed BEGIN must return the operation permit"
        );
    }

    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn failed_cached_reader_rollback_deregisters_only_when_connection_is_discarded() {
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
            path: Some(dir.path().join("sql_bridge_reader_failed_rollback.db")),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin_view = database_tx_view(&pool);
        let conn = open_standalone_reader(&pool).unwrap();
        let mut reader = SqliteReader {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: None,
                read_transaction_slot: None,
            }),
            pool: Arc::clone(&pool),
            poisoned: false,
        };

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("BEGIN must establish the registered transaction");
        let opened = khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("the admitted transaction must be registered");
        reader
            .handle
            .as_ref()
            .expect("reader must retain its connection")
            .conn
            .authorizer(Some(deny_rollback))
            .unwrap();

        let rollback = reader
            .query_all(SqlStatement {
                sql: "ROLLBACK".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(rollback.is_err(), "the authorizer must reject ROLLBACK");
        assert_eq!(
            khive_storage::tx_registry::oldest_for(&origin_view)
                .expect("failed ROLLBACK must retain registry evidence")
                .id,
            opened.id
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            0,
            "failed ROLLBACK must retain reader admission"
        );

        drop(reader);
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "discarding the connection must not leak its registry entry"
        );
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
    }

    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn cached_read_only_handles_reject_unsupported_transaction_control_without_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(
                dir.path()
                    .join("sql_bridge_reader_unsupported_tx_control.db"),
            ),
            write_queue_enabled: Some(true),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let mut reader = bridge.reader().await.unwrap();
        for (sql, keyword) in [
            ("BEGIN IMMEDIATE", "BEGIN"),
            ("BEGIN EXCLUSIVE", "BEGIN"),
            // Trailing-mode spellings parse in SQLite as a NAMED deferred
            // transaction, but the mode keyword in name position reads as
            // lock intent; the classifier must refuse rather than launder
            // them into a deferred start the cached reader would then hold.
            ("BEGIN TRANSACTION IMMEDIATE", "BEGIN"),
            ("BEGIN TRANSACTION EXCLUSIVE", "BEGIN"),
            ("BEGIN DEFERRED TRANSACTION trailing", "BEGIN"),
            // Quoted/bracketed tails tokenize as no identifier at all, so a
            // classifier that stops at the tokenizer's `None` reads them as
            // an accepted form's end. They must be refused exactly like the
            // bare-word spellings.
            ("BEGIN TRANSACTION \"IMMEDIATE\"", "BEGIN"),
            ("BEGIN TRANSACTION [IMMEDIATE]", "BEGIN"),
            ("BEGIN TRANSACTION `IMMEDIATE`", "BEGIN"),
            ("BEGIN TRANSACTION 'IMMEDIATE'", "BEGIN"),
            ("BEGIN \"DEFERRED\"", "BEGIN"),
            ("BEGIN; COMMIT", "BEGIN"),
            ("START TRANSACTION", "START"),
            ("COMMIT", "COMMIT"),
        ] {
            let rejected = reader
                .query_all(SqlStatement {
                    sql: sql.into(),
                    params: vec![],
                    label: None,
                })
                .await;
            assert!(
                matches!(
                    &rejected,
                    Err(StorageError::InvalidInput { message, .. })
                        if message.contains(keyword)
                ),
                "unsupported cached-reader control {sql:?} must fail closed; got {rejected:?}"
            );
            assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
        }

        let mut queue_backed_writer = bridge.writer().await.unwrap();
        let rejected = queue_backed_writer
            .query_all(SqlStatement {
                sql: "SAVEPOINT stale_snapshot".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput {
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "writer.query_all"
                    && message.contains("transaction control")
                    && message.contains("SAVEPOINT")
            ),
            "a queue-backed writer without an explicit read transaction must reject nested \
             transaction control; got {rejected:?}"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "queue-backed rejection must leave the operation permit available"
        );
        let value = queue_backed_writer
            .query_scalar(SqlStatement {
                sql: "SELECT 8".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("transaction-control rejection must not consume the queue-backed handle");
        assert!(matches!(value, Some(SqlValue::Integer(8))));

        queue_backed_writer
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("queue-backed cached reader must share explicit read-transaction admission");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
        queue_backed_writer
            .query_all(SqlStatement {
                sql: "END".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("END must release queue-backed cached-reader admission");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
    }

    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn dropping_cached_reader_transaction_closes_snapshot_before_releasing_permit() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_tx_drop.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(20),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin_view = database_tx_view(&pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();
        let mut contender = bridge.reader().await.unwrap();

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("begin admitted transaction");
        reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("materialize read snapshot");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
            "the live snapshot must remain registered until handle drop"
        );

        drop(reader);
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "handle drop must close SQLite before deregistering the snapshot"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "dropping the handle must close its transaction before returning admission"
        );
        contender
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("a new operation must run after the transactional handle drops");
    }

    /// #1846 regression: a cached-reader explicit read transaction that is
    /// never explicitly finished (a stuck/leaked caller that keeps reusing
    /// the handle without COMMIT/ROLLBACK) would otherwise pin the WAL
    /// snapshot open for as long as the caller kept calling in. Without the
    /// age check this reddens: the second `query_all` would return the row
    /// materialized inside the still-open transaction instead of an error,
    /// and `tx_registry::oldest_for` would keep reporting the same span
    /// open past `read_tx_max_age`.
    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn expired_cached_reader_transaction_is_rolled_back_on_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_tx_max_age.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(20),
            read_tx_max_age: std::time::Duration::from_millis(20),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin_view = database_tx_view(&pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("begin admitted transaction");
        reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("materialize read snapshot");
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
            "the open transaction must be registered before it ages out"
        );

        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        let evictions_before = crate::checkpoint::read_tx_max_age_evictions();
        let error = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect_err("reusing a transaction past read_tx_max_age must be refused");
        assert!(
            error.is_retryable(),
            "an evicted-transaction error must be retryable so the caller can open a fresh \
             snapshot: {error}"
        );
        match &error {
            StorageError::ReadTransactionAgeEvicted {
                operation,
                max_age_secs,
            } => {
                assert_eq!(operation.as_ref(), "query_all");
                assert_eq!(
                    *max_age_secs, 0,
                    "a 20ms read_tx_max_age truncates to 0 whole seconds"
                );
            }
            other => panic!(
                "a clean age-triggered rollback must surface the dedicated \
                 ReadTransactionAgeEvicted variant, not a generic classification: {other:?}"
            ),
        }
        assert_eq!(
            crate::checkpoint::read_tx_max_age_evictions(),
            evictions_before + 1,
            "the eviction must be counted in the #1846 diagnostics gauge"
        );
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "the expired transaction must be rolled back and deregistered rather than \
             continuing to pin the WAL snapshot"
        );

        reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("the handle must remain usable for a fresh autocommit read after eviction");
    }

    /// #1846 follow-up: the age-eviction branch
    /// has a rollback-failure path distinct from
    /// `failed_cached_reader_rollback_deregisters_only_when_connection_is_discarded`
    /// above (which covers an explicit caller-issued `ROLLBACK`, not the
    /// age-triggered cleanup rollback). When SQLite denies the age-triggered
    /// `ROLLBACK`, the branch must still discard the poisoned connection,
    /// deregister the expired transaction span, and release the reader
    /// admission permit rather than leaking either.
    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn expired_cached_reader_transaction_rollback_denial_discards_connection_and_releases_admission(
    ) {
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
            path: Some(
                dir.path()
                    .join("sql_bridge_reader_tx_max_age_rollback_denied.db"),
            ),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(20),
            read_tx_max_age: std::time::Duration::from_millis(20),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin_view = database_tx_view(&pool);
        let conn = open_standalone_reader(&pool).unwrap();
        let mut reader = SqliteReader {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: None,
                read_transaction_slot: None,
            }),
            pool: Arc::clone(&pool),
            poisoned: false,
        };

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("begin admitted transaction");
        reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("materialize read snapshot");
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
            "the open transaction must be registered before it ages out"
        );

        reader
            .handle
            .as_ref()
            .expect("reader must retain its connection")
            .conn
            .authorizer(Some(deny_rollback))
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        let evictions_before = crate::checkpoint::read_tx_max_age_evictions();
        let error = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect_err("a denied rollback on an expired transaction must surface an error");
        assert!(
            error.is_retryable(),
            "even a failed cleanup rollback must remain classified retryable so callers open a \
             fresh handle: {error}"
        );
        match &error {
            StorageError::ReadTransactionAgeEvictionCleanupFailed {
                operation,
                max_age_secs,
                message,
            } => {
                assert_eq!(operation.as_ref(), "query_all");
                assert_eq!(
                    *max_age_secs, 0,
                    "a 20ms read_tx_max_age truncates to 0 whole seconds"
                );
                assert!(
                    message.contains("rollback failed"),
                    "the failure must be attributable to the denied ROLLBACK, not silent \
                     success: {message}"
                );
            }
            other => panic!(
                "a denied cleanup rollback must surface the dedicated \
                 ReadTransactionAgeEvictionCleanupFailed variant, not a generic Transaction \
                 error the caller cannot machine-detect: {other:?}"
            ),
        }
        assert_eq!(
            crate::checkpoint::read_tx_max_age_evictions(),
            evictions_before + 1,
            "the eviction attempt must still be counted even though cleanup failed"
        );
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
            "a denied rollback must discard the connection and deregister the expired \
             transaction span rather than leaking it"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "discarding the poisoned connection must release the reader admission slot"
        );

        let reuse = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM sqlite_schema".into(),
                params: vec![],
                label: None,
            })
            .await;
        let message = match reuse {
            Err(StorageError::Pool { message, .. }) => message,
            other => panic!(
                "reusing this discarded reader must fail loudly with 'connection already \
                 consumed' rather than silently reopening; got {other:?}"
            ),
        };
        assert!(
            message.contains("connection already consumed"),
            "expected the discarded reader's reuse error to name the pinned failure; got \
             {message:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(tx_registry)]
    async fn cancelled_cached_reader_transaction_releases_guards_after_connection_closes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_tx_drop_cancel.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(50),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let origin_view = database_tx_view(&pool);
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = SqliteReader {
            handle: Some(
                open_explicit_read_transaction_handle(Arc::clone(&pool))
                    .await
                    .unwrap(),
            ),
            pool: Arc::clone(&pool),
            poisoned: false,
        };
        let mut contender = bridge.reader().await.unwrap();

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("begin admitted transaction");
        assert!(
            khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
            "the explicit transaction must be registered before cancellation"
        );
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);

        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let query = tokio::spawn(crate::scope_test_read_progress(
            Arc::clone(&progress),
            async move { reader.query_all(deliberately_slow_read_statement()).await },
        ));
        wait_for_progress(progress.as_ref()).await;
        query.abort();
        assert!(matches!(query.await, Err(error) if error.is_cancelled()));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while khive_storage::tx_registry::oldest_for(&origin_view).is_some()
                || pool.sql_bridge_reader_slots().available_permits() != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection cleanup leaked transaction evidence or reader admission");
        contender
            .query_all(SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("admission must recover after the cancelled connection closes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(tx_registry)]
    async fn cancelled_cached_reader_rolls_back_releases_wal_and_clears_handler() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_tx_request_cancel.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(500),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let writer = open_standalone_writer(&pool).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE snapshot_probe(id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO snapshot_probe(value) VALUES ('seed');",
            )
            .unwrap();
        let mut reader = bridge.reader().await.unwrap();
        let mut contender = bridge.reader().await.unwrap();

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("begin admitted transaction");
        reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM snapshot_probe".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("materialize a real WAL snapshot");
        writer
            .execute_batch(
                "WITH RECURSIVE rows(value) AS (\
                 SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 100\
                 ) INSERT INTO snapshot_probe(value) SELECT printf('row-%d', value) FROM rows;",
            )
            .unwrap();
        let (_, log_before, checkpointed_before) = passive_checkpoint(&writer);
        assert!(
            log_before > checkpointed_before,
            "the explicit reader snapshot must pin WAL frames before cancellation"
        );

        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let query = tokio::spawn(crate::scope_test_read_progress(
            Arc::clone(&progress),
            crate::scope_request_read_cancellation(cancel_rx, async move {
                let result = reader.query_all(deliberately_slow_read_statement()).await;
                (reader, result)
            }),
        ));
        wait_for_progress(progress.as_ref()).await;
        cancel_tx.send(true).unwrap();
        let (mut reader, result) = tokio::time::timeout(std::time::Duration::from_secs(1), query)
            .await
            .expect("interrupted explicit read transaction did not stop promptly")
            .unwrap();
        assert!(
            matches!(result, Err(StorageError::Timeout { .. })),
            "request cancellation must surface as a typed timeout; got {result:?}"
        );
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);

        let (_, log_after, checkpointed_after) = passive_checkpoint(&writer);
        assert_eq!(
            log_after, checkpointed_after,
            "cancellation must release the explicit reader's WAL snapshot"
        );
        contender
            .query_all(SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("the sole reader permit must be reusable after rollback");

        let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
        reader
            .query_all(SqlStatement {
                sql: "WITH RECURSIVE rows(value) AS (\
                      SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 10000\
                      ) SELECT SUM(value) FROM rows"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("same connection must remain usable after handler teardown");
        assert_eq!(
            progress.load(std::sync::atomic::Ordering::SeqCst),
            stopped_at,
            "the cancelled request's progress callback bled into the next borrower"
        );
    }

    /// A statement that blocks inside a single SQLite VM step for longer
    /// than [`crate::read_cancellation::DEFAULT_SQLITE_INTERRUPT_GRACE_MS`].
    /// Unlike a recursive CTE (interrupt-checked every 1,000 VM
    /// instructions, so it stops promptly), a UDF call is one opcode: SQLite
    /// cannot observe the interrupt flag until the call returns. This is the
    /// only way to deterministically force a worker past the grace window
    /// rather than merely past `wait_for_progress`.
    fn register_khive_test_slow_udf(
        conn: &rusqlite::Connection,
        sleep_ms: u64,
        started: Arc<std::sync::atomic::AtomicBool>,
    ) {
        conn.create_scalar_function(
            "khive_test_slow_udf",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |_| {
                started.store(true, std::sync::atomic::Ordering::Release);
                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                Ok(0i64)
            },
        )
        .unwrap();
    }

    async fn wait_for_flag(flag: &std::sync::atomic::AtomicBool) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !flag.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("slow UDF never started");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_read_past_grace_recovers_admission_after_bounded_join() {
        // Regression for the PR #1897 review blocker: a `spawn_blocking`
        // read worker that outlives `KHIVE_SQLITE_INTERRUPT_GRACE_MS` must
        // not be treated as reaped just because the async side detached
        // from it. This forces the worker past grace with a slow UDF (so
        // the interrupt genuinely cannot be observed mid-call) and asserts
        // that admission and the WAL snapshot are only reported recovered
        // once the real worker has actually joined.
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_grace_exceeded.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(2_000),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let writer = open_standalone_writer(&pool).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE grace_probe(id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO grace_probe(value) VALUES ('seed');",
            )
            .unwrap();

        let mut reader = SqliteReader {
            handle: Some(
                open_explicit_read_transaction_handle(Arc::clone(&pool))
                    .await
                    .unwrap(),
            ),
            pool: Arc::clone(&pool),
            poisoned: false,
        };
        let mut contender = SqliteReader {
            handle: Some(
                open_explicit_read_transaction_handle(Arc::clone(&pool))
                    .await
                    .unwrap(),
            ),
            pool: Arc::clone(&pool),
            poisoned: false,
        };
        let udf_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        register_khive_test_slow_udf(
            &reader.handle.as_ref().unwrap().conn,
            900,
            Arc::clone(&udf_started),
        );

        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("begin admitted transaction");
        reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM grace_probe".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("materialize a real WAL snapshot");
        writer
            .execute_batch(
                "WITH RECURSIVE rows(value) AS (\
                 SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 100\
                 ) INSERT INTO grace_probe(value) SELECT printf('row-%d', value) FROM rows;",
            )
            .unwrap();
        let (_, log_before, checkpointed_before) = passive_checkpoint(&writer);
        assert!(
            log_before > checkpointed_before,
            "the explicit reader snapshot must pin WAL frames before cancellation"
        );

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let query = tokio::spawn(crate::scope_request_read_cancellation(
            cancel_rx,
            async move {
                let result = reader
                    .query_all(SqlStatement {
                        sql: "SELECT khive_test_slow_udf()".into(),
                        params: vec![],
                        label: None,
                    })
                    .await;
                (reader, result)
            },
        ));
        // Wait for the UDF to actually be running (not just scheduled) so
        // registration has completed and the worker is provably blocked
        // inside SQLite before cancelling — the same proof `wait_for_progress`
        // gives the recursive-CTE tests, since a progress callback (fired
        // between opcodes) never runs during the UDF's own blocking call.
        wait_for_flag(udf_started.as_ref()).await;
        cancel_tx.send(true).unwrap();

        let (mut reader, result) = tokio::time::timeout(std::time::Duration::from_secs(3), query)
            .await
            .expect(
                "a worker that settles within the grace+hard-cap bound must not hang the caller",
            )
            .unwrap();
        assert!(
            matches!(result, Err(StorageError::Timeout { .. })),
            "request cancellation must still surface as a typed timeout even after grace \
             was exceeded; got {result:?}"
        );

        // The response was only returned above because the real worker
        // joined — expected-fail arm: this assertion would fail (permit
        // still 0) under the pre-fix behavior, which detached and returned
        // Timeout at the grace boundary while the worker (and its permit)
        // were still live.
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "the sole reader permit must be visible again once the bounded join completes"
        );

        let (_, log_after, checkpointed_after) = passive_checkpoint(&writer);
        assert_eq!(
            log_after, checkpointed_after,
            "the abandoned explicit read transaction must release its WAL snapshot by the \
             time the caller observes the timeout"
        );

        contender
            .query_all(SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("a fresh reader must be admitted once the zombie worker has settled");

        // The settled connection itself must also be reusable (not
        // quarantined) and must not still be carrying the slow UDF's
        // progress callback into a later borrower.
        reader
            .query_all(SqlStatement {
                sql: "SELECT 1".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("the interrupted connection must remain usable after settling");
    }

    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn cached_reader_transaction_lifecycle_survives_sqlite_empty_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_prefixed_tx_control.db")),
            write_queue_enabled: Some(true),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut reader = bridge.reader().await.unwrap();

        reader
            .query_all(SqlStatement {
                sql: " ; -- empty statement\n /* leading comment */ \u{feff} BEGIN DEFERRED".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("prefixed BEGIN must enter the admitted transaction state");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
        reader
            .query_all(SqlStatement {
                sql: " /* leading comment */ \u{feff} ; COMMIT".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("prefixed COMMIT must end the admitted transaction state");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);

        let rejected = reader
            .query_all(SqlStatement {
                sql: " ; /* no active transaction */ \u{feff} COMMIT".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput {
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "query_all"
                    && message.contains("transaction control")
                    && message.contains("COMMIT")
            ),
            "a prefixed COMMIT without an admitted transaction must still fail closed; \
             got {rejected:?}"
        );

        let mut queue_backed_writer = bridge.writer().await.unwrap();
        let rejected = queue_backed_writer
            .query_all(SqlStatement {
                sql: "-- leading comment\n \u{feff} ; /* empty */ SAVEPOINT pinned".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput {
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "writer.query_all"
                    && message.contains("transaction control")
                    && message.contains("SAVEPOINT")
            ),
            "a queue-backed cached reader must classify transaction control through \
             comments, BOMs, and empty statements; got {rejected:?}"
        );

        let value = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 10".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("prefixed transaction lifecycle must preserve the cached reader");
        assert!(matches!(value, Some(SqlValue::Integer(10))));
    }

    #[tokio::test]
    async fn cached_reader_restores_autocommit_before_releasing_its_operation_permit() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_autocommit.db")),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let conn = open_standalone_reader(&pool).unwrap();
        conn.execute_batch("BEGIN DEFERRED; SELECT * FROM sqlite_schema")
            .unwrap();
        assert!(
            !conn.is_autocommit(),
            "the regression precondition needs a live read transaction"
        );
        let mut reader = SqliteReader {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: None,
                read_transaction_slot: None,
            }),
            pool: Arc::clone(&pool),
            poisoned: false,
        };

        let rejected = reader
            .query_all(SqlStatement {
                // The stale-state cleanup must take precedence over the
                // ordinary transaction-control rejection. Otherwise an idle
                // snapshot could survive every rejected ROLLBACK attempt.
                sql: "ROLLBACK".into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput {
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "query_all"
                    && message.contains("outside autocommit")
            ),
            "a cached reader that reaches the boundary outside autocommit must fail closed; \
             got {rejected:?}"
        );
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            1,
            "the permit may be released only after the stale transaction is gone"
        );
        assert!(
            reader.handle.is_none(),
            "the restored connection must close instead of surviving as an idle standalone cache"
        );

        let value = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 9".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("the cleaned reader must remain usable through the pooled route");
        assert!(matches!(value, Some(SqlValue::Integer(9))));
    }

    #[tokio::test]
    async fn standalone_writer_read_preserves_manual_atomic_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_writer_atomic_read.db")),
            write_queue_enabled: Some(false),
            max_readers: 1,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let writer = pool.writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE atomic_read_test \
                     (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
                )
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), true);

        let observed = bridge
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO atomic_read_test (id, value) VALUES (1, 'pending')"
                                .into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    let count = writer
                        .query_scalar(SqlStatement {
                            sql: "SELECT COUNT(*) FROM atomic_read_test".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
                })
            }))
            .await
            .expect("manual atomic read must not be mistaken for an idle reader snapshot");
        let observed = match observed.downcast::<Option<SqlValue>>() {
            Ok(observed) => observed,
            Err(_) => panic!("unexpected atomic result type"),
        };
        assert!(matches!(*observed, Some(SqlValue::Integer(1))));

        let mut reader = bridge.reader().await.unwrap();
        let committed = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM atomic_read_test".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(matches!(committed, Some(SqlValue::Integer(1))));
    }

    #[tokio::test]
    async fn request_cancellation_preserves_file_backed_manual_atomic_read_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("sql_bridge_writer_tx_cancel.db")),
                write_queue_enabled: Some(false),
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE writer_tx_cancel_probe(\
                 id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
            )
            .unwrap();
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let observed = crate::scope_request_read_cancellation(
            cancel_rx,
            bridge.atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO writer_tx_cancel_probe VALUES (1, 'before')".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    cancel_tx.send(true).unwrap();
                    let count = writer
                        .query_scalar(SqlStatement {
                            sql: "SELECT COUNT(*) FROM writer_tx_cancel_probe".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO writer_tx_cancel_probe VALUES (2, 'after')".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
                })
            })),
        )
        .await
        .expect("request cancellation must not interrupt an admitted manual write transaction");
        let observed = match observed.downcast::<Option<SqlValue>>() {
            Ok(observed) => observed,
            Err(_) => panic!("unexpected atomic result type"),
        };
        assert!(matches!(*observed, Some(SqlValue::Integer(1))));

        let reader = pool.reader().unwrap();
        let rows: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM writer_tx_cancel_probe", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2, "both writes around the SELECT must commit");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_standalone_writer_transaction_retains_active_reader_admission() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("sql_bridge_writer_tx_admission.db")),
                write_queue_enabled: Some(false),
                max_readers: 1,
                checkout_timeout: std::time::Duration::from_millis(250),
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        let writer_slot = pool
            .sql_bridge_writer_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, _completed) = blocking_non_interrupting_progress_gate(&conn);
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(writer_slot),
                read_transaction_slot: None,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        khive_storage::SqlWriter::execute(
            &mut writer,
            SqlStatement {
                sql: "BEGIN IMMEDIATE".into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .unwrap();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        cancel_tx.send(true).unwrap();

        let query = tokio::spawn(crate::scope_request_read_cancellation(
            cancel_rx,
            async move {
                let result =
                    khive_storage::SqlReader::query_all(&mut writer, progress_gate_statement())
                        .await;
                let rollback = khive_storage::SqlWriter::execute(
                    &mut writer,
                    SqlStatement {
                        sql: "ROLLBACK".into(),
                        params: vec![],
                        label: None,
                    },
                )
                .await;
                (result, rollback)
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .expect("cancelled writer-transaction SELECT never reached SQLite");
        assert_eq!(
            pool.sql_bridge_reader_slots().available_permits(),
            0,
            "a writer-supertrait SELECT must retain ordinary active-reader admission"
        );

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        let (rows, rollback) = tokio::time::timeout(std::time::Duration::from_secs(2), query)
            .await
            .expect("writer-transaction SELECT did not finish after its gate opened")
            .unwrap();
        assert_eq!(
            rows.expect("request cancellation interrupted the admitted writer transaction")
                .len(),
            1
        );
        rollback.expect("writer transaction did not return to autocommit");
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
    }

    #[tokio::test]
    async fn expired_deadline_preserves_pool_backed_manual_atomic_read_and_commit() {
        let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE pool_writer_tx_deadline_probe(\
                 id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
            )
            .unwrap();
        let bridge = SqlBridge::new(Arc::clone(&pool), false);

        let observed = crate::scope_request_read_deadline(
            std::time::Duration::ZERO,
            bridge.atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO pool_writer_tx_deadline_probe VALUES (1, 'before')"
                                .into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    let count = writer
                        .query_scalar(SqlStatement {
                            sql: "SELECT COUNT(*) FROM pool_writer_tx_deadline_probe".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO pool_writer_tx_deadline_probe VALUES (2, 'after')"
                                .into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
                })
            })),
        )
        .await
        .expect("an expired read deadline must not interrupt an admitted manual write transaction");
        let observed = match observed.downcast::<Option<SqlValue>>() {
            Ok(observed) => observed,
            Err(_) => panic!("unexpected atomic result type"),
        };
        assert!(matches!(*observed, Some(SqlValue::Integer(1))));

        let reader = pool.reader().unwrap();
        let rows: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM pool_writer_tx_deadline_probe",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "both writes around the SELECT must commit");
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
    async fn abandoned_writer_read_interrupts_and_releases_writer_handle() {
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
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let query = tokio::spawn(crate::scope_test_read_progress(
            Arc::clone(&progress),
            async move {
                khive_storage::SqlReader::query_all(&mut writer, deliberately_slow_read_statement())
                    .await
            },
        ));

        wait_for_progress(progress.as_ref()).await;
        query.abort();
        assert!(matches!(query.await, Err(error) if error.is_cancelled()));
        let writer_after =
            tokio::time::timeout(std::time::Duration::from_millis(500), bridge.writer())
                .await
                .expect("abandoned SQLite read did not release the writer handle promptly")
                .expect("writer handle remained unavailable after read interruption");
        drop(writer_after);
        let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            progress.load(std::sync::atomic::Ordering::SeqCst),
            stopped_at,
            "writer-backed SQLite read kept consuming work after cancellation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_cancellation_never_interrupts_admitted_execute_batch() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_cancelled_writer_batch.db")),
            write_queue_enabled: Some(false),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE cancellation_write_probe(\
                     id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
                )
                .unwrap();
        }

        let handle_slot = pool
            .sql_bridge_writer_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, completed) = blocking_non_interrupting_progress_gate(&conn);
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let query = tokio::spawn(crate::scope_request_read_cancellation(
            cancel_rx,
            async move {
                khive_storage::SqlWriter::execute_batch(&mut writer, vec![slow_insert_statement()])
                    .await
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .expect("mutating execute_batch never reached SQLite VM work");
        cancel_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !query.is_finished(),
            "request-read cancellation must not interrupt an admitted batch"
        );

        let contender = bridge.writer().await;
        let retained_slot = matches!(
            &contender,
            Err(StorageError::AdmissionTimeout { operation, .. })
                if operation.as_ref() == "sql_bridge.writer_handle"
        );
        drop(contender);

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        let affected = tokio::time::timeout(std::time::Duration::from_secs(2), query)
            .await
            .expect("admitted batch did not finish after its gate was released")
            .unwrap()
            .expect("request cancellation must preserve the batch result");
        assert_eq!(affected, 10_000);
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("completed batch did not release its connection");
        assert!(
            retained_slot,
            "request cancellation released the writer slot before the admitted batch stopped"
        );
        let reader = pool.reader().unwrap();
        let count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM cancellation_write_probe", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 10_000, "the admitted batch must commit every row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_cancellation_never_interrupts_dml_returning_via_sql_reader() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_dml_returning_cancel.db")),
            write_queue_enabled: Some(false),
            checkout_timeout: std::time::Duration::from_millis(250),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch(
                    "CREATE TABLE returning_write_probe(\
                     id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
                )
                .unwrap();
        }

        let handle_slot = pool
            .sql_bridge_writer_slots()
            .acquire_owned()
            .await
            .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, completed) = blocking_non_interrupting_progress_gate(&conn);
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
            }),
            writer_task: None,
            origin: pool.origin(),
            db: crate::timeout_sink::db_label(&pool),
            pool: Arc::clone(&pool),
        };
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let query = tokio::spawn(crate::scope_request_read_cancellation(
            cancel_rx,
            async move {
                khive_storage::SqlReader::query_all(
                    &mut writer,
                    SqlStatement {
                        sql: "INSERT INTO returning_write_probe(value) \
                          WITH RECURSIVE rows(value) AS (\
                          SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 10000\
                          ) SELECT value FROM rows RETURNING id"
                            .into(),
                        params: vec![],
                        label: Some("non-interruptible-returning-probe".into()),
                    },
                )
                .await
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .expect("DML RETURNING never reached admitted SQLite work");
        cancel_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !query.is_finished(),
            "request-read cancellation interrupted DML RETURNING"
        );

        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        let rows = tokio::time::timeout(std::time::Duration::from_secs(2), query)
            .await
            .expect("DML RETURNING did not finish after its gate was released")
            .unwrap()
            .expect("request cancellation must preserve DML RETURNING's result");
        assert_eq!(rows.len(), 10_000);
        tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
            .await
            .expect("completed DML RETURNING did not release its connection");

        let reader = pool.reader().unwrap();
        let count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM returning_write_probe", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 10_000, "DML RETURNING must commit every row");
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
            SlotTimeoutClass::Admission,
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let (entered, release, completed) = blocking_non_interrupting_progress_gate(&conn);
        let writer = Arc::new(tokio::sync::Mutex::new(SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
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
            SlotTimeoutClass::Admission,
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
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
    async fn standalone_execute_batch_rejects_prefixed_commit_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(dir.path().join("sql_bridge_prefixed_commit_standalone.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch("CREATE TABLE prefixed_commit (id INTEGER PRIMARY KEY)")
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let mut writer = bridge.writer().await.unwrap();

        let rejected = writer
            .execute_batch(vec![
                SqlStatement {
                    sql: "INSERT INTO prefixed_commit (id) VALUES (1)".into(),
                    params: vec![],
                    label: None,
                },
                SqlStatement {
                    sql: " ; -- empty statement\n /* leading comment */ \u{feff} ; COMMIT".into(),
                    params: vec![],
                    label: None,
                },
            ])
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput {
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "execute_batch"
                    && message.contains("transaction control")
                    && message.contains("COMMIT")
            ),
            "standalone execute_batch must reject a prefixed COMMIT before the INSERT; \
             got {rejected:?}"
        );

        let mut reader = bridge.reader().await.unwrap();
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM prefixed_commit".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(0))),
            "prefixed COMMIT rejection must happen before the earlier INSERT; got {count:?}"
        );

        let affected = writer
            .execute(SqlStatement {
                sql: "INSERT INTO prefixed_commit (id) VALUES (2)".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("prefixed COMMIT rejection must leave the standalone handle reusable");
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
            (" ; BEGIN", Some("BEGIN")),
            (" ; ; -- empty\n /* comment */ COMMIT", Some("COMMIT")),
            ("  \u{feff} SAVEPOINT sp1", Some("SAVEPOINT")),
            ("/* comment */ \u{feff} ; RELEASE sp1", Some("RELEASE")),
            ("\u{feff} ; \u{feff} -- empty\n ROLLBACK", Some("ROLLBACK")),
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
            (" ; /* empty statements only */ ; ", None),
            (" ; SELECT 1", None),
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
    fn cached_read_transaction_control_classification_matrix() {
        use CachedReadTransactionControl::{BeginDeferred, Finish, Unsupported};

        for (sql, expected) in [
            ("BEGIN", Some(BeginDeferred)),
            ("begin transaction", Some(BeginDeferred)),
            (
                "/* p */ \u{feff} ; BEGIN /* mode */ DEFERRED",
                Some(BeginDeferred),
            ),
            ("BEGIN DEFERRED TRANSACTION", Some(BeginDeferred)),
            ("BEGIN IMMEDIATE", Some(Unsupported("BEGIN"))),
            ("BEGIN /* lock */ EXCLUSIVE", Some(Unsupported("BEGIN"))),
            ("BEGIN TRANSACTION IMMEDIATE", Some(Unsupported("BEGIN"))),
            ("begin transaction exclusive", Some(Unsupported("BEGIN"))),
            ("BEGIN TRANSACTION DEFERRED", Some(Unsupported("BEGIN"))),
            ("BEGIN IMMEDIATE TRANSACTION", Some(Unsupported("BEGIN"))),
            ("BEGIN TRANSACTION named_txn", Some(Unsupported("BEGIN"))),
            (
                "BEGIN DEFERRED TRANSACTION trailing",
                Some(Unsupported("BEGIN")),
            ),
            ("BEGIN DEFERRED DEFERRED", Some(Unsupported("BEGIN"))),
            // Non-identifier tails: the tokenizer yields no token for a
            // quoted, bracketed, or backticked tail, which must read as a
            // refused remainder, never as end-of-statement.
            (
                "BEGIN TRANSACTION \"IMMEDIATE\"",
                Some(Unsupported("BEGIN")),
            ),
            ("BEGIN TRANSACTION [IMMEDIATE]", Some(Unsupported("BEGIN"))),
            ("BEGIN TRANSACTION `IMMEDIATE`", Some(Unsupported("BEGIN"))),
            ("BEGIN TRANSACTION 'IMMEDIATE'", Some(Unsupported("BEGIN"))),
            ("BEGIN \"DEFERRED\"", Some(Unsupported("BEGIN"))),
            ("BEGIN; COMMIT", Some(Unsupported("BEGIN"))),
            // Trailing empty statements and trivia remain an accepted end.
            ("BEGIN;", Some(BeginDeferred)),
            ("BEGIN DEFERRED ; -- done", Some(BeginDeferred)),
            ("BEGIN TRANSACTION /* t */ ;;", Some(BeginDeferred)),
            ("START TRANSACTION", Some(Unsupported("START"))),
            ("COMMIT", Some(Finish("COMMIT"))),
            ("END TRANSACTION", Some(Finish("END"))),
            ("ROLLBACK", Some(Finish("ROLLBACK"))),
            ("ROLLBACK TRANSACTION", Some(Finish("ROLLBACK"))),
            ("ROLLBACK TO sp", Some(Unsupported("ROLLBACK"))),
            (
                "ROLLBACK /* nested */ TRANSACTION /* target */ TO sp",
                Some(Unsupported("ROLLBACK")),
            ),
            ("SAVEPOINT sp", Some(Unsupported("SAVEPOINT"))),
            ("SELECT 1", None),
        ] {
            assert_eq!(
                cached_read_transaction_control(sql),
                expected,
                "cached-reader transaction classification mismatch for {sql:?}"
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

        let prefixed = khive_storage::SqlWriter::execute_batch(
            &mut *writer,
            vec![
                SqlStatement {
                    sql: "INSERT INTO tx_reject_queue_test (id, val) VALUES (3, 'prefixed')".into(),
                    params: vec![],
                    label: None,
                },
                SqlStatement {
                    sql: "/* leading */ \u{feff} ; -- empty\n ; COMMIT".into(),
                    params: vec![],
                    label: None,
                },
            ],
        )
        .await;
        assert!(
            matches!(
                &prefixed,
                Err(StorageError::InvalidInput {
                    operation,
                    message,
                    ..
                }) if operation.as_ref() == "execute_batch"
                    && message.contains("transaction control")
                    && message.contains("COMMIT")
            ),
            "a prefixed COMMIT must be rejected before touching the writer task; got {prefixed:?}"
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
            SlotTimeoutClass::Admission,
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        conn.authorizer(Some(deny_rollback)).unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
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
            SlotTimeoutClass::Admission,
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
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
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
            SlotTimeoutClass::Admission,
        )
        .await
        .unwrap();
        let conn = open_standalone_writer(&pool).unwrap();
        let mut writer = SqliteWriter {
            handle: Some(StandaloneHandle {
                conn,
                _retained_slot: Some(handle_slot),
                read_transaction_slot: None,
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
                Err(StorageError::AdmissionTimeout { operation, .. })
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

    /// Queue-backed ordinary reads use the pooled reader budget rather than a
    /// cached standalone connection. Arm 1 saturates the one reader permit and
    /// proves the failure is a visible pooled-admission timeout. Arm 2 proves
    /// a handle with no exceptional transaction connection remains reusable
    /// through the pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_backed_read_uses_pool_budget_and_remains_reusable_after_saturation() {
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
                Err(StorageError::AdmissionTimeout { operation, .. })
                    if operation.as_ref() == "sql_bridge.reader_operation"
            ),
            "queue-backed read with reader permits saturated must time out \
             at the shared pooled-reader admission stage; \
             got {starved:?}"
        );
        let saturated = pool.reader_acquisition_snapshot();
        assert_eq!(saturated.checkout_timeouts, 1);
        assert_eq!(saturated.standalone_opens, 0);
        drop(held);

        // Arm 2: a queue-backed handle in the exact post-cancelled-read
        // state — `handle: None` — must serve the next ordinary read through
        // the pool, never a hard "connection already consumed" failure (that
        // contract is specific to a poisoned explicit-transaction reader).
        // Constructed directly so the state is deterministic.
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
            .expect("read on a queue-backed handle with no transaction connection must pool")
            .expect("seeded row must be visible");
        assert!(
            matches!(&row.columns[0].value, SqlValue::Text(v) if v == "seed"),
            "pooled read must return the seeded row; got {:?}",
            row.columns[0].value
        );
    }

    /// ADR-136 D1 gate 3 amendment: `SqlWriter::query_row`/`query_all` carry
    /// no read-only restriction at the trait level — a caller could hand a
    /// DML-with-RETURNING statement to `query_row`. A queue-backed handle now
    /// sends ordinary reads through a pooled read-only connection, so SQLite
    /// rejects the statement instead of mutating on an untracked writer.
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
