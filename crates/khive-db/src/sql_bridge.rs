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
    acquire_standalone_lease, acquire_unit_lease, admit_standalone_operation, StandaloneWriteError,
};
mod standalone_batch;
#[cfg(test)]
use standalone_batch::BatchPoisonReason;
use standalone_batch::{
    run_standalone_batch, run_standalone_script, run_standalone_statement,
    run_standalone_top_level, BatchFailure, PoisonedBatchError,
};

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

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

/// Settle a pooled call that left its connection inside a transaction, before
/// the guard is released. The guard is checked out for this call only, so the
/// transaction cannot be continued by a later call: it is rolled back here and
/// the call fails, rather than the guard's drop rolling it back after the call
/// has already reported success.
///
/// The outcome, in order: a rollback that cannot prove autocommit retires the
/// writer and the call fails with `WriterSettlementUnknown` (side effects
/// unknown), whatever the call itself returned; a call that failed keeps its own error once its
/// transaction is rolled back; a call that succeeded fails with
/// `InvalidInput`.
fn settle_pooled_call<T>(
    guard: &crate::pool::WriterGuard<'_>,
    operation: &'static str,
    result: khive_storage::types::StorageResult<T>,
) -> khive_storage::types::StorageResult<T> {
    if guard.is_autocommit() {
        return result;
    }
    if let Err(settlement) = guard.rollback_or_retire("pooled call left its transaction open") {
        if let Err(error) = &result {
            tracing::warn!(
                operation,
                %error,
                "pooled call failed inside a transaction it opened, and its rollback \
                 could not prove autocommit; reporting the settlement failure"
            );
        }
        return Err(settlement.into_storage_error(StorageCapability::Sql, operation));
    }
    result?;
    Err(StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: operation.into(),
        message: "the call left a transaction open; it was rolled back before the pooled \
                  writer was released — use atomic_unit to run statements as one transaction"
            .into(),
    })
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

/// Skip a fully parenthesized region starting at `rest[0] == b'('`,
/// respecting SQL string/identifier quoting (`'...'` with `''` escapes,
/// `"..."`/`` `...` `` with doubled-quote escapes, and `[...]` bracket
/// identifiers) and line/block comments, so that a `)`, `(`, or `--` that
/// merely appears inside a literal or comment is never mistaken for syntax.
/// Returns the bytes after the matching `)`, or `None` if the region never
/// closes (malformed or truncated SQL) — callers must fail closed on `None`.
fn skip_balanced_parens(rest: &[u8]) -> Option<&[u8]> {
    debug_assert_eq!(rest.first(), Some(&b'('));
    let mut depth: u32 = 0;
    let mut idx = 0;
    loop {
        match *rest.get(idx)? {
            b'(' => {
                depth += 1;
                idx += 1;
            }
            b')' => {
                depth -= 1;
                idx += 1;
                if depth == 0 {
                    return Some(&rest[idx..]);
                }
            }
            quote @ (b'\'' | b'"' | b'`') => {
                idx += 1;
                loop {
                    match *rest.get(idx)? {
                        byte if byte == quote => {
                            idx += 1;
                            if rest.get(idx) == Some(&quote) {
                                idx += 1; // doubled-quote escape inside the literal
                            } else {
                                break;
                            }
                        }
                        _ => idx += 1,
                    }
                }
            }
            b'[' => {
                idx += 1;
                while *rest.get(idx)? != b']' {
                    idx += 1;
                }
                idx += 1;
            }
            b'-' if rest.get(idx + 1) == Some(&b'-') => {
                idx += 2;
                while idx < rest.len() && rest[idx] != b'\n' {
                    idx += 1;
                }
            }
            b'/' if rest.get(idx + 1) == Some(&b'*') => {
                idx += 2;
                while idx + 1 < rest.len() && !(rest[idx] == b'*' && rest[idx + 1] == b'/') {
                    idx += 1;
                }
                idx = (idx + 2).min(rest.len());
            }
            _ => idx += 1,
        }
    }
}

/// Skip one SQLite identifier — unquoted (`[A-Za-z0-9_]+`, as
/// [`next_sqlite_token`] already recognizes), double-quoted or
/// backtick-quoted (both honoring the doubled-quote-character escape), or
/// bracket-quoted (no escape; SQLite's bracket form ends at the first `]`) —
/// and return the bytes after it. A quoted CTE name may contain any byte,
/// including `(` and `)`, so the CTE-list walk must skip the identifier
/// itself rather than tokenizing it the way an unquoted keyword is.
fn skip_sqlite_identifier(rest: &[u8]) -> Option<&[u8]> {
    match *rest.first()? {
        quote @ (b'"' | b'`') => {
            let mut idx = 1;
            loop {
                match *rest.get(idx)? {
                    byte if byte == quote => {
                        idx += 1;
                        if rest.get(idx) == Some(&quote) {
                            idx += 1; // doubled-quote escape inside the identifier
                        } else {
                            break;
                        }
                    }
                    _ => idx += 1,
                }
            }
            Some(&rest[idx..])
        }
        b'[' => {
            let mut idx = 1;
            while *rest.get(idx)? != b']' {
                idx += 1;
            }
            Some(&rest[idx + 1..])
        }
        _ => next_sqlite_token(rest).map(|(_, next)| next),
    }
}

/// Walk past the common-table-expression list following `WITH [RECURSIVE]`
/// and return the bytes starting at the main statement's own head keyword.
/// Each CTE body is skipped as a balanced parenthesized region
/// ([`skip_balanced_parens`]), so nested parens, string literals, and
/// comments inside a CTE body never confuse the walk. Returns `None` if the
/// CTE list is not well-formed enough to walk past safely — the caller must
/// fail closed (refuse admission) rather than guess.
fn skip_common_table_expressions(tail: &[u8]) -> Option<&[u8]> {
    let mut rest = skip_sqlite_empty_prefix(tail);
    if let Some((word, next)) = next_sqlite_token(rest) {
        if word.eq_ignore_ascii_case(b"RECURSIVE") {
            rest = skip_sqlite_empty_prefix(next);
        }
    }
    loop {
        // CTE name — unquoted or SQLite-quoted (`"..."`, `` `...` ``, `[...]`).
        let next = skip_sqlite_identifier(rest)?;
        rest = skip_sqlite_empty_prefix(next);
        // Optional column-name list.
        if rest.first() == Some(&b'(') {
            rest = skip_sqlite_empty_prefix(skip_balanced_parens(rest)?);
        }
        let (as_keyword, next) = next_sqlite_token(rest)?;
        if !as_keyword.eq_ignore_ascii_case(b"AS") {
            return None;
        }
        rest = skip_sqlite_empty_prefix(next);
        // Optional `[NOT] MATERIALIZED` hint (SQLite 3.35+).
        if let Some((word, next)) = next_sqlite_token(rest) {
            if word.eq_ignore_ascii_case(b"MATERIALIZED") {
                rest = skip_sqlite_empty_prefix(next);
            } else if word.eq_ignore_ascii_case(b"NOT") {
                let (materialized, next) = next_sqlite_token(skip_sqlite_empty_prefix(next))?;
                if !materialized.eq_ignore_ascii_case(b"MATERIALIZED") {
                    return None;
                }
                rest = skip_sqlite_empty_prefix(next);
            }
        }
        // CTE body.
        if rest.first() != Some(&b'(') {
            return None;
        }
        rest = skip_sqlite_empty_prefix(skip_balanced_parens(rest)?);
        if rest.first() == Some(&b',') {
            rest = skip_sqlite_empty_prefix(&rest[1..]);
            continue;
        }
        return Some(rest);
    }
}

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
/// and the fixed `PRAGMA` allow-lists above. A `WITH` clause is not itself a
/// read shape — SQLite also allows `WITH ... INSERT/UPDATE/DELETE`, with or
/// without `RETURNING` — so a `WITH` head is admitted only after walking past
/// its CTE list ([`skip_common_table_expressions`]) and confirming the main
/// statement underneath is itself `SELECT` or `VALUES`.
pub(crate) fn reader_capability_admits(sql: &str) -> Result<(), String> {
    let rest = skip_sqlite_empty_prefix(sql.as_bytes());
    let Some((head, tail)) = next_sqlite_token(rest) else {
        // No head token (empty/comment-only statement): let SQLite's own
        // prepare step surface that error rather than duplicating it here.
        return Ok(());
    };
    if head.eq_ignore_ascii_case(b"SELECT") || head.eq_ignore_ascii_case(b"VALUES") {
        return Ok(());
    }
    if head.eq_ignore_ascii_case(b"WITH") {
        let Some(after_ctes) = skip_common_table_expressions(tail) else {
            return Err(
                "WITH statement's common-table-expression list could not be parsed; refusing \
                 to admit it through the reader capability"
                    .into(),
            );
        };
        return match next_sqlite_token(after_ctes) {
            Some((main_head, _))
                if main_head.eq_ignore_ascii_case(b"SELECT")
                    || main_head.eq_ignore_ascii_case(b"VALUES") =>
            {
                Ok(())
            }
            other => Err(format!(
                "WITH ... {:?} is not admitted through the reader capability; only a \
                 read-only SELECT/VALUES body after the CTE list may run against a pooled \
                 reader connection",
                other.map_or_else(
                    || "<none>".to_string(),
                    |(main_head, _)| String::from_utf8_lossy(main_head).into_owned()
                )
            )),
        };
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
/// standalone writer to `StorageError::Timeout`; ordinary reader-pool
/// saturation reports the typed `AdmissionTimeout` through
/// `ConnectionPool::resolve_reader_checkout`.
///
/// Classification is decided by this enum and never by the operation label.
/// The label names the caller's own read (`query_row`, `writer.query_all`, and
/// so on) so that a recorded maximum reader hold can be attributed to the read
/// that caused it.
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

fn open_standalone_reader(pool: &ConnectionPool) -> Result<rusqlite::Connection, StorageError> {
    pool.open_standalone_reader(StandaloneReaderPurpose::ExplicitSqlReadTransaction)
        .map_err(|error| StorageError::driver(StorageCapability::Sql, "open_reader", error))
}

#[cfg(test)]
fn open_standalone_writer(pool: &ConnectionPool) -> Result<rusqlite::Connection, StorageError> {
    let conn = pool
        .open_standalone_writer()
        .map_err(|e| e.into_storage_error(StorageCapability::Sql, "open_writer"))?;
    configure_standalone_writer(pool, conn)
}

fn open_admitted_standalone_writer(
    pool: &ConnectionPool,
) -> Result<rusqlite::Connection, StorageError> {
    let conn = pool
        .open_standalone_writer_for_admitted_operation()
        .map_err(|e| e.into_storage_error(StorageCapability::Sql, "open_writer"))?;
    configure_standalone_writer(pool, conn)
}

fn configure_standalone_writer(
    pool: &ConnectionPool,
    conn: rusqlite::Connection,
) -> Result<rusqlite::Connection, StorageError> {
    let config = pool.config();
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

/// Open the writer handle without a premature capacity sample. Its later
/// operation owns admission; a cold handle must allow a recovery checkpoint.
/// See [`open_standalone_reader_on_blocking`] for the blocking rationale.
async fn open_standalone_writer_on_blocking(
    pool: Arc<ConnectionPool>,
    slot: OwnedSemaphorePermit,
) -> khive_storage::types::StorageResult<(rusqlite::Connection, OwnedSemaphorePermit)> {
    open_standalone_on_blocking(pool, slot, "open_writer", open_admitted_standalone_writer).await
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

// =============================================================================
// File-backed: SqliteWriter (standalone connection)
// =============================================================================

struct SqliteWriter {
    // Manual atomic owners observe the final unit, excluding inner/cleanup errors.
    observe_direct_errors: bool,
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
    event_rows: Option<Arc<AtomicEventRows>>,
    /// The volume lease an enclosing manual atomic unit holds for its whole
    /// transaction, so the unit's own statements do not request it again.
    /// Declared last: the connection closes before the lease is released.
    held_lease: Option<crate::disk_guard::DetachedVolumeLease>,
}

fn execute_top_level_maintenance(
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
        // on the writer task, transaction-control rejection remains an
        // `execute_batch` contract; the standalone branch below refuses it
        // unless a unit holds the volume lease, because this primitive is also
        // used by internal atomic transaction owners.
        // route it through the writer task when available. `self.handle` is
        // left untouched so a subsequent `execute`/`execute_script` call on
        // this same handle still works over the standalone connection.
        if let Some(writer_task) = self.writer_task.clone() {
            let event_rows = self.event_rows.clone();
            return writer_task
                .send_bounded(move |conn| {
                    let mut stmt = prepare_cached_sql_statement(conn, &statement.sql)
                        .map_err(|e| map_rusqlite_err(e, "execute"))?;
                    bind_params(&mut stmt, &statement.params)
                        .map_err(|e| map_rusqlite_err(e, "execute"))?;
                    let affected = stmt
                        .raw_execute()
                        .map_err(|e| map_rusqlite_err(e, "execute"))?;
                    if let Some(event_rows) = event_rows.as_deref() {
                        event_rows.observe(&statement, affected as u64);
                    }
                    Ok(affected as u64)
                })
                .await;
        }

        self.require_standalone_handle("execute")?;
        let unit_holds_lease = self.held_lease.is_some();
        // ADR-154: a standalone statement holds the volume lease only for the
        // call, so a transaction it opened would continue after the lease was
        // released. Only a unit that already holds the lease for its whole
        // span (`atomic_unit`'s manual transaction) may send transaction
        // control through `execute`.
        if !unit_holds_lease {
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
        }
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute".into(),
            message: "connection already consumed".into(),
        })?;
        let event_rows = self.event_rows.clone();
        let pool = Arc::clone(&self.pool);
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_statement(handle, pool, unit_holds_lease, statement, event_rows)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute", e))?;
        self.handle = Some(handle);
        let affected = result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(error) => self.map_direct_error(error, "execute"),
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
            return writer_task
                .send_bounded(move |conn| {
                    let prepared = prepare_batch_statements(conn, &statements)
                        .map_err(|e| map_rusqlite_err(e, "execute_batch"))?;
                    execute_prepared_batch(conn, prepared, &statements, event_rows.as_deref())
                        .map_err(|e| map_rusqlite_err(e, "execute_batch"))
                })
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
        let unit_holds_lease = self.held_lease.is_some();
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_batch(
                handle,
                pool,
                unit_holds_lease,
                statements,
                origin,
                event_rows,
            )
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
                .send_bounded(move |conn| {
                    conn.execute_batch(&script)
                        .map_err(|e| map_rusqlite_err(e, "execute_script"))
                })
                .await;
        }

        self.require_standalone_handle("execute_script")?;
        let handle = self.handle.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script".into(),
            message: "connection already consumed".into(),
        })?;
        let pool = Arc::clone(&self.pool);
        let unit_holds_lease = self.held_lease.is_some();
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_script(handle, pool, unit_holds_lease, script)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script", e))?;
        self.handle = handle;
        result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(error) => self.map_direct_error(error, "execute_script"),
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
                    .map_err(|e| map_rusqlite_err(e, "execute_script_top_level"))
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
        let unit_holds_lease = self.held_lease.is_some();
        let (handle, result) = tokio::task::spawn_blocking(move || {
            run_standalone_top_level(handle, pool, unit_holds_lease, maintenance)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script_top_level", e))?;
        self.handle = Some(handle);
        result.map_err(|failure| match failure {
            StandaloneWriteError::Refused(error) => error,
            StandaloneWriteError::Sql(error) => {
                self.map_direct_error(error, "execute_script_top_level")
            }
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
    /// Present only while an admitted explicit deferred read transaction
    /// (ADR-005/ADR-091) is open on the in-memory backend's single shared
    /// connection. Retaining it here — instead of drawing a fresh checkout
    /// per call, the way an ordinary read does — is what gives the span
    /// real connection ownership: see [`SharedReaderTransactionGuard`] and
    /// [`run_pool_backed_reader_query`].
    transaction: Option<SharedReaderTransactionGuard>,
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
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute", e)
            })?;
            let result = (|| {
                let mut stmt = prepare_cached_sql_statement(&guard, &statement.sql)
                    .map_err(|e| map_rusqlite_err(e, "pool_writer.execute"))?;
                bind_params(&mut stmt, &statement.params)
                    .map_err(|e| map_rusqlite_err(e, "pool_writer.execute"))?;
                let rows = stmt
                    .raw_execute()
                    .map_err(|e| map_rusqlite_err(e, "pool_writer.execute"))?;
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
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_batch", e)
            })?;
            let result = (|| {
                let prepared = prepare_batch_statements(&guard, &statements)
                    .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
                guard
                    .execute_batch("BEGIN IMMEDIATE")
                    .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
                let _tx_handle = khive_storage::tx_registry::register_scoped(
                    Some("pool_writer.execute_batch".to_string()),
                    pool.origin(),
                );
                let result = execute_prepared_batch(&guard, prepared, &statements, None)
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
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_script", e)
            })?;
            let result = guard
                .execute_batch(&script)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_script"));
            settle_pooled_call(&guard, "pool_writer.execute_script", result)
                .inspect_err(|error| pool.record_direct_writer_error(error))
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
/// used to drive an [`AtomicUnitOp`] on the queued or in-memory path, where the
/// closure body runs inside a `spawn_blocking` (synchronous
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
    event_rows: Option<Arc<AtomicEventRows>>,
    conn: *const rusqlite::Connection,
}

// SAFETY: `InlineWriter` is never actually shared across a real thread
// boundary — it is constructed, driven to completion synchronously via
// `block_on_sync`, and dropped within a single call frame inside the
// `spawn_blocking` closure (see `atomic_unit`). The `Send`
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
        if let Some(event_rows) = self.event_rows.as_deref() {
            event_rows.observe(&statement, affected as u64);
        }
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
        execute_prepared_batch(
            self.conn(),
            prepared,
            &statements,
            self.event_rows.as_deref(),
        )
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
    pub fn new(pool: Arc<ConnectionPool>, _is_file_backed: bool) -> Self {
        // The legacy hint is not an authority for choosing an unguarded
        // in-memory writer. A caller cannot route a file-backed pool through
        // PoolBackedWriter by supplying `false`.
        let is_file_backed = pool.canonical_path().is_some();
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
                transaction: None,
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
                observe_direct_errors: true,
                event_rows: None,
                handle,
                writer_task,
                origin: self.pool.origin(),
                db,
                pool: Arc::clone(&self.pool),
                held_lease: None,
            }))
        } else {
            Ok(Box::new(PoolBackedWriter {
                pool: Arc::clone(&self.pool),
            }))
        }
    }

    /// Implements the trait's atomic-unit suspend-free invariant
    /// (`SqlAccess::atomic_unit`'s doc comment): on the queued and in-memory branches,
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
        let event_rows = Arc::new(AtomicEventRows::default());
        let result = async {
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
                    let pending_event_rows = Arc::clone(&event_rows);
                    return writer_task
                        .send_bounded(move |conn| {
                            let mut inline = InlineWriter {
                                event_rows: Some(Arc::clone(&pending_event_rows)),
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
                // The unit's lease spans its BEGIN, statements and COMMIT or
                // ROLLBACK, as one queued request does on the write-queue path.
                let unit_lease = acquire_unit_lease(Arc::clone(&self.pool)).await?;
                let (conn, handle_slot) =
                    open_standalone_writer_on_blocking(Arc::clone(&self.pool), handle_slot).await?;
                let mut writer = SqliteWriter {
                    observe_direct_errors: false,
                    event_rows: Some(Arc::clone(&event_rows)),
                    handle: Some(StandaloneHandle {
                        conn,
                        _retained_slot: Some(handle_slot),
                        read_transaction_slot: None,
                    }),
                    writer_task: None,
                    origin: self.pool.origin(),
                    db: crate::timeout_sink::db_label(&self.pool),
                    pool: Arc::clone(&self.pool),
                    held_lease: unit_lease,
                };
                run_manual_atomic_unit(&mut writer, op, self.pool.origin())
                    .await
                    .inspect_err(|error| self.pool.record_direct_writer_error(error))
            } else {
                // Every statement shares one connection. Keep its guard through
                // commit/rollback so other units and ordinary writes cannot join it.
                let pool = Arc::clone(&self.pool);
                let pending_event_rows = Arc::clone(&event_rows);
                tokio::task::spawn_blocking(move || {
                    let guard = pool.try_writer().map_err(|error: SqliteError| {
                        StorageError::driver(StorageCapability::Sql, "atomic_unit", error)
                    })?;
                    let conn = guard.conn();
                    if !conn.is_autocommit() {
                        pool.retire_pooled_writer(conn);
                        return Err(StorageError::writer_task_terminated(
                            khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                        ));
                    }
                    if let Err(error) = conn.execute_batch("BEGIN IMMEDIATE") {
                        if !conn.is_autocommit() {
                            pool.retire_pooled_writer(conn);
                            return Err(StorageError::writer_task_terminated(
                                khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                            ));
                        }
                        return Err(map_rusqlite_err(error, "atomic_unit.begin"))
                            .inspect_err(|error| pool.record_direct_writer_error(error));
                    }
                    let _tx_handle = khive_storage::tx_registry::register_scoped(
                        Some("atomic_unit".to_string()),
                        pool.origin(),
                    );
                    let (result, terminal_state) = crate::writer_task::execute_wrapped_transaction(
                        conn,
                        "atomic_unit.commit",
                        |conn| {
                            let mut inline = InlineWriter {
                                event_rows: Some(Arc::clone(&pending_event_rows)),
                                conn: conn as *const rusqlite::Connection,
                            };
                            block_on_sync(op(&mut inline)).and_then(|result| result)
                        },
                    );
                    if terminal_state.is_some() {
                        pool.retire_pooled_writer(conn);
                    }
                    result.inspect_err(|error| pool.record_direct_writer_error(error))
                })
                .await
                .map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "atomic_unit", error)
                })?
            }
        }
        .await;
        khive_storage::usage::account_event_write(
            result.as_ref().map(|_| event_rows.committed_rows()),
        );
        result
    }
}

#[cfg(test)]
#[path = "sql_bridge_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "sql_bridge/direct_busy_tests.rs"]
mod direct_busy_tests;

#[cfg(test)]
#[path = "sql_bridge/settlement_hazard_tests.rs"]
mod settlement_hazard_tests;
