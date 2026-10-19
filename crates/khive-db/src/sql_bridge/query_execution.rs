use super::{
    bind_params, prepare_sql_statement, row_to_sql_row, PageRequest, SqlRow, SqlStatement,
    SqliteError, StorageCapability, StorageError,
};

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
pub(super) fn settle_pooled_call<T>(
    guard: &crate::pool::PooledAutocommitWriteUnit<'_>,
    operation: &'static str,
    result: khive_storage::types::StorageResult<T>,
) -> khive_storage::types::StorageResult<T> {
    if guard.conn().is_autocommit() {
        return result;
    }
    if let Err(settlement) = guard.rollback_or_retire("pooled call left its transaction open") {
        if let Err(error) = &result {
            tracing::warn!(target: "khive_db::sql_bridge",
                operation,
                %error,
                "pooled call failed inside a transaction it opened, and its rollback \
                 could not prove autocommit; reporting the settlement failure"
            );
        }
        let settlement = match &result {
            Err(original) => SqliteError::settlement_with_cause(
                original,
                khive_storage::error::SqliteWriteStage::Statement,
            ),
            Ok(_) => settlement,
        };
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

pub(super) fn prepare_bound_statement<'conn>(
    conn: &'conn rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<rusqlite::Statement<'conn>, rusqlite::Error> {
    let mut stmt = prepare_sql_statement(conn, &statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;
    Ok(stmt)
}

pub(super) fn execute_prepared_query(
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

pub(super) fn execute_prepared_query_row(
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

pub(super) fn execute_prepared_query_page(
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
pub(super) fn execute_query(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    execute_prepared_query(prepare_bound_statement(conn, statement)?)
}

pub(super) fn execute_query_row(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Option<SqlRow>, rusqlite::Error> {
    execute_prepared_query_row(prepare_bound_statement(conn, statement)?)
}

pub(super) fn execute_query_page(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    page: &PageRequest,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    execute_prepared_query_page(prepare_bound_statement(conn, statement)?, page)
}

/// Map a rusqlite error to `StorageError`.
pub(super) fn map_write_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    crate::error::statement_failure(map_rusqlite_err(e, op))
}

pub(super) fn map_write_boundary_err(
    e: rusqlite::Error,
    op: &'static str,
    stage: khive_storage::error::SqliteWriteStage,
) -> StorageError {
    crate::error::with_write_stage(map_rusqlite_err(e, op), stage)
}

pub(super) fn map_rusqlite_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Sql, op, e)
}
