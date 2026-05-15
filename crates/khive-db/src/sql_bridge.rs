//! SqlAccess bridge: connects `ConnectionPool` to `khive_storage::SqlAccess`.
//!
//! Two modes:
//! - **File-backed**: Opens standalone connections per reader/writer/tx call (high concurrency).
//! - **Memory**: Uses pool-backed approach (acquire pool connection per-query inside `spawn_blocking`).

use std::sync::Arc;

use async_trait::async_trait;

use khive_storage::error::StorageError;
use khive_storage::types::{SqlColumn, SqlRow, SqlStatement, SqlTxOptions, SqlValue};
use khive_storage::StorageCapability;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

// =============================================================================
// Shared helpers
// =============================================================================

/// Convert a rusqlite `Row` into an owned `SqlRow`.
fn row_to_sql_row(row: &rusqlite::Row<'_>, col_count: usize, col_names: &[String]) -> SqlRow {
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

/// Bind `SqlValue` parameters to a rusqlite statement.
fn bind_params(
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

/// Execute a query on a `rusqlite::Connection` and return owned rows.
fn execute_query(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&statement.sql)?;
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

/// Map a rusqlite error to `StorageError`.
fn map_rusqlite_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Sql, op, e)
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
    let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
        operation: "writer".into(),
        message: "in-memory databases do not support standalone writer; use pool-backed".into(),
    })?;

    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| map_rusqlite_err(e, "open_writer"))?;

    conn.busy_timeout(config.busy_timeout)
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;
    conn.pragma_update(None, "cache_size", "-65536")
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;
    conn.pragma_update(None, "mmap_size", "1073741824")
        .map_err(|e| map_rusqlite_err(e, "open_writer"))?;

    Ok(conn)
}

// =============================================================================
// File-backed: SqliteReader (standalone connection)
// =============================================================================

struct SqliteReader {
    conn: Option<rusqlite::Connection>,
}

#[async_trait]
impl khive_storage::SqlReader for SqliteReader {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "query_row".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&conn, &statement);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "query_row", e))?;
        self.conn = Some(conn);
        let rows = result.map_err(|e| map_rusqlite_err(e, "query_row"))?;
        Ok(rows.into_iter().next())
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "query_all".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&conn, &statement);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "query_all", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "query_all"))
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
    conn: Option<rusqlite::Connection>,
}

#[async_trait]
impl khive_storage::SqlReader for SqliteWriter {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "writer.query_row".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&conn, &statement);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "writer.query_row", e))?;
        self.conn = Some(conn);
        let rows = result.map_err(|e| map_rusqlite_err(e, "writer.query_row"))?;
        Ok(rows.into_iter().next())
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "writer.query_all".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&conn, &statement);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "writer.query_all", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "writer.query_all"))
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
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "execute".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = (|| -> Result<usize, rusqlite::Error> {
                let mut stmt = conn.prepare(&statement.sql)?;
                bind_params(&mut stmt, &statement.params)?;
                stmt.raw_execute()
            })();
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute", e))?;
        self.conn = Some(conn);
        let affected = result.map_err(|e| map_rusqlite_err(e, "execute"))?;
        Ok(affected as u64)
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::types::StorageResult<u64> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_batch".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<u64, rusqlite::Error> {
                conn.execute_batch("BEGIN IMMEDIATE")?;
                let mut total: u64 = 0;
                for statement in &statements {
                    let mut stmt = conn.prepare(&statement.sql)?;
                    bind_params(&mut stmt, &statement.params)?;
                    total += stmt.raw_execute()? as u64;
                }
                conn.execute_batch("COMMIT")?;
                Ok(total)
            })();
            if result.is_err() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            (conn, result)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_batch", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "execute_batch"))
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = conn.execute_batch(&script);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "execute_script"))
    }
}

// =============================================================================
// File-backed: SqliteTransaction (standalone connection)
// =============================================================================

struct SqliteTransaction {
    conn: Option<rusqlite::Connection>,
}

#[async_trait]
impl khive_storage::SqlReader for SqliteTransaction {
    async fn query_row(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlRow>> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "tx.query_row".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&conn, &statement);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "tx.query_row", e))?;
        self.conn = Some(conn);
        let rows = result.map_err(|e| map_rusqlite_err(e, "tx.query_row"))?;
        Ok(rows.into_iter().next())
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "tx.query_all".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = execute_query(&conn, &statement);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "tx.query_all", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "tx.query_all"))
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
impl khive_storage::SqlWriter for SqliteTransaction {
    async fn execute(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<u64> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "tx.execute".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = (|| -> Result<usize, rusqlite::Error> {
                let mut stmt = conn.prepare(&statement.sql)?;
                bind_params(&mut stmt, &statement.params)?;
                stmt.raw_execute()
            })();
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "tx.execute", e))?;
        self.conn = Some(conn);
        let affected = result.map_err(|e| map_rusqlite_err(e, "tx.execute"))?;
        Ok(affected as u64)
    }

    async fn execute_batch(
        &mut self,
        statements: Vec<SqlStatement>,
    ) -> khive_storage::types::StorageResult<u64> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "tx.execute_batch".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let mut total: u64 = 0;
            for statement in &statements {
                let res = (|| -> Result<usize, rusqlite::Error> {
                    let mut stmt = conn.prepare(&statement.sql)?;
                    bind_params(&mut stmt, &statement.params)?;
                    stmt.raw_execute()
                })();
                match res {
                    Ok(n) => total += n as u64,
                    Err(e) => return (conn, Err(e)),
                }
            }
            (conn, Ok(total))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "tx.execute_batch", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "tx.execute_batch"))
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "tx.execute_script".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = conn.execute_batch(&script);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "tx.execute_script", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "tx.execute_script"))
    }
}

#[async_trait]
impl khive_storage::SqlTransaction for SqliteTransaction {
    async fn commit(mut self: Box<Self>) -> khive_storage::types::StorageResult<()> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Transaction {
            operation: "commit".into(),
            message: "connection already consumed".into(),
        })?;
        tokio::task::spawn_blocking(move || {
            conn.execute_batch("COMMIT")
                .map_err(|e| map_rusqlite_err(e, "commit"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "commit", e))?
    }

    async fn rollback(mut self: Box<Self>) -> khive_storage::types::StorageResult<()> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Transaction {
            operation: "rollback".into(),
            message: "connection already consumed".into(),
        })?;
        tokio::task::spawn_blocking(move || {
            conn.execute_batch("ROLLBACK")
                .map_err(|e| map_rusqlite_err(e, "rollback"))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "rollback", e))?
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
            let rows = execute_query(&guard, &statement)
                .map_err(|e| map_rusqlite_err(e, "pool_reader.query_row"))?;
            Ok(rows.into_iter().next())
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
            let rows = execute_query(&guard, &statement)
                .map_err(|e| map_rusqlite_err(e, "pool_writer.query_row"))?;
            Ok(rows.into_iter().next())
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
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute", e)
            })?;
            let mut stmt = guard
                .prepare(&statement.sql)
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
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e: SqliteError| {
                StorageError::driver(StorageCapability::Sql, "pool_writer.execute_batch", e)
            })?;
            guard
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
            let result = (|| -> Result<u64, StorageError> {
                let mut total = 0u64;
                for statement in &statements {
                    let mut stmt = guard
                        .prepare(&statement.sql)
                        .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
                    bind_params(&mut stmt, &statement.params)
                        .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?;
                    total += stmt
                        .raw_execute()
                        .map_err(|e| map_rusqlite_err(e, "pool_writer.execute_batch"))?
                        as u64;
                }
                Ok(total)
            })();
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
// SqlBridge: the SqlAccess implementor
// =============================================================================

/// Bridges `ConnectionPool` to `khive_storage::SqlAccess`.
///
/// Dispatches based on whether the pool is file-backed or in-memory:
/// - File-backed: standalone connections per reader/writer/tx (high concurrency).
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
            let conn = open_standalone_reader(&self.pool)?;
            Ok(Box::new(SqliteReader { conn: Some(conn) }))
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
            let conn = open_standalone_writer(&self.pool)?;
            Ok(Box::new(SqliteWriter { conn: Some(conn) }))
        } else {
            Ok(Box::new(PoolBackedWriter {
                pool: Arc::clone(&self.pool),
            }))
        }
    }

    async fn begin_tx(
        &self,
        _options: SqlTxOptions,
    ) -> khive_storage::types::StorageResult<Box<dyn khive_storage::SqlTransaction>> {
        // Transactions need a standalone connection so the BEGIN/COMMIT state
        // is not shared with other operations. For in-memory DBs we still
        // open a standalone writer since the pool writer would conflict.
        let conn = if self.is_file_backed {
            open_standalone_writer(&self.pool)?
        } else {
            return Err(StorageError::Pool {
                operation: "begin_tx".into(),
                message: "transactions require file-backed database (not in-memory)".into(),
            });
        };

        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| map_rusqlite_err(e, "begin_tx"))?;
        Ok(Box::new(SqliteTransaction { conn: Some(conn) }))
    }
}
