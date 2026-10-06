use super::{AtomicU64, Ordering, SqlColumn, SqlRow, SqlStatement, SqlValue};

/// Convert a rusqlite `Row` into an owned `SqlRow`.
pub(super) fn row_to_sql_row(
    row: &rusqlite::Row<'_>,
    col_count: usize,
    col_names: &[String],
) -> SqlRow {
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
    pub(super) static ROW_CONVERSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
pub(super) fn prepare_sql_statement<'conn>(
    conn: &'conn rusqlite::Connection,
    sql: &str,
) -> Result<rusqlite::Statement<'conn>, rusqlite::Error> {
    conn.prepare(sql)
}

/// Prepare one [`SqlStatement`] through rusqlite's per-connection LRU cache
/// while retaining the same single-statement tail validation as
/// [`prepare_sql_statement`].
pub(super) fn prepare_cached_sql_statement<'conn>(
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
pub(super) enum PreparedBatchStatement<'conn> {
    Ready(rusqlite::Statement<'conn>),
    PrepareAtExecution,
}

/// Prepare each batch statement exactly once while rejecting an executable
/// tail before any statement runs.
pub(super) fn prepare_batch_statements<'conn>(
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

/// Actual event-row insertions observed inside one owned atomic transaction.
/// Labels come from the canonical event statement builders, not SQL parsing.
/// Observation rows and unrelated DML never contribute. This accumulator crosses
/// the blocking writer boundary; usage is published on the caller task only after
/// the transaction owner reports a committed result.
#[derive(Default)]
pub(super) struct AtomicEventRows(AtomicU64);

pub(super) const COUNTED_EVENT_INSERT_LABELS: &[&str] = &[
    "event_insert_on_writer",
    "hard-delete-derived_from-warning",
    "hard-delete-supersedes-warning",
    "hard-delete-precedes-warning",
    "hard-delete-supports-warning",
    "hard-delete-refutes-warning",
];

impl AtomicEventRows {
    pub(super) fn observe(&self, statement: &SqlStatement, affected: u64) {
        if statement
            .label
            .as_deref()
            .is_some_and(|label| COUNTED_EVENT_INSERT_LABELS.contains(&label))
        {
            let _ = self
                .0
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                    Some(total.saturating_add(affected))
                });
        }
    }

    pub(super) fn committed_rows(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
