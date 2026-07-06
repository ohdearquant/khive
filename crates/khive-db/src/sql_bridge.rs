//! SqlAccess bridge: connects `ConnectionPool` to `khive_storage::SqlAccess`.
//!
//! Two modes:
//! - **File-backed**: Opens standalone connections per reader/writer call (high concurrency).
//!   Cross-statement atomicity goes through `atomic_unit`, which drives a single
//!   registered raw transaction span rather than a caller-held per-tx connection.
//! - **Memory**: Uses pool-backed approach (acquire pool connection per-query inside `spawn_blocking`).

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;

use khive_storage::error::StorageError;
use khive_storage::types::{SqlColumn, SqlRow, SqlStatement, SqlValue};
use khive_storage::{AtomicUnitOp, StorageCapability};

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
    /// ADR-067 Component A: when the write queue is enabled, `execute_batch`
    /// routes the whole caller-supplied statement list through the
    /// single-writer task instead of opening its own `BEGIN IMMEDIATE` on
    /// `conn`. `None` when the flag is off or no writer task is available
    /// (best-effort — degrades to the standalone-connection path below).
    writer_task: Option<crate::writer_task::WriterTaskHandle>,
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
        // ADR-067 Component A (Fork C slice 2): a single statement is
        // self-contained, just like `execute_batch`'s full statement list —
        // route it through the writer task when available. `self.conn` is
        // left untouched so a subsequent `execute`/`execute_script` call on
        // this same handle still works over the standalone connection.
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send(move |conn| {
                    let mut stmt = conn
                        .prepare(&statement.sql)
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
        // ADR-067 Component A: this call is self-contained (the full statement
        // list is supplied up front and the whole thing commits or rolls back
        // as one unit) — unlike `writer()`'s live incrementally-driven handle,
        // it maps cleanly onto a single `WriteRequest`. Route it through the
        // writer task when available; `self.conn` is left untouched so a
        // subsequent `execute`/`execute_script` call on this same handle still
        // works over the standalone connection (that dispatch is unmigrated —
        // see `SqlBridge::writer()`).
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send(move |conn| {
                    let mut total: u64 = 0;
                    for statement in &statements {
                        let mut stmt = conn
                            .prepare(&statement.sql)
                            .map_err(|e| map_rusqlite_err(e, "execute_batch"))?;
                        bind_params(&mut stmt, &statement.params)
                            .map_err(|e| map_rusqlite_err(e, "execute_batch"))?;
                        total += stmt
                            .raw_execute()
                            .map_err(|e| map_rusqlite_err(e, "execute_batch"))?
                            as u64;
                    }
                    Ok(total)
                })
                .await;
        }

        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_batch".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
                return (conn, Err(e));
            }
            // Registered only after BEGIN succeeds, so an unopened transaction is
            // never counted. The handle is declared here — enclosing both the
            // statement-execution closure below AND the ROLLBACK path — so it
            // stays registered until the transaction is actually finished
            // (COMMIT or ROLLBACK), not just until the inner closure returns.
            let _tx_handle =
                khive_storage::tx_registry::register(Some("execute_batch".to_string()));
            let result = (|| -> Result<u64, rusqlite::Error> {
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
            // `_tx_handle` drops here, after ROLLBACK (or COMMIT) has already run.
            (conn, result)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_batch", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "execute_batch"))
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
        // ADR-067 Component A (Fork C slice 2): the script text is
        // self-contained (supplied up front, runs as one unit), just like
        // `execute_batch` — route it through the writer task when
        // available. `self.conn` is left untouched so a subsequent
        // `execute`/`execute_script` call on this same handle still works
        // over the standalone connection. Callers must supply a DML-only
        // script (no bare `BEGIN`/`COMMIT`/`ROLLBACK`) on the flag-on path,
        // since it runs inside the writer task's own transaction — same
        // contract as `execute_batch`'s statement list.
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send(move |conn| {
                    conn.execute_batch(&script)
                        .map_err(|e| map_rusqlite_err(e, "execute_script"))
                })
                .await;
        }

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

    async fn execute_script_top_level(
        &mut self,
        script: String,
    ) -> khive_storage::types::StorageResult<()> {
        // ADR-067 Component A (Fork C slice 2 round 2, BLOCKER A): unlike
        // `execute_script`, this must NOT run inside the writer task's
        // per-request `BEGIN IMMEDIATE` — statements such as VACUUM are
        // rejected by SQLite inside any open transaction. Route through
        // `WriterTaskHandle::send_top_level`, which still serializes this
        // call through the single writer owner but skips the transaction
        // wrap entirely.
        if let Some(writer_task) = self.writer_task.clone() {
            return writer_task
                .send_top_level(move |conn| {
                    conn.execute_batch(&script)
                        .map_err(|e| map_rusqlite_err(e, "execute_script_top_level"))
                })
                .await;
        }

        // Flag off / no writer task: identical to `execute_script`'s own
        // flag-off path — a bare `execute_batch` on the standalone
        // connection, already transaction-free.
        let conn = self.conn.take().ok_or_else(|| StorageError::Pool {
            operation: "execute_script_top_level".into(),
            message: "connection already consumed".into(),
        })?;
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let res = conn.execute_batch(&script);
            (conn, res)
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sql, "execute_script_top_level", e))?;
        self.conn = Some(conn);
        result.map_err(|e| map_rusqlite_err(e, "execute_script_top_level"))
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
            let _tx_handle =
                khive_storage::tx_registry::register(Some("pool_writer.execute_batch".to_string()));
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
        let rows = execute_query(self.conn(), &statement)
            .map_err(|e| map_rusqlite_err(e, "inline.query_row"))?;
        Ok(rows.into_iter().next())
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
        execute_query(self.conn(), &statement).map_err(|e| map_rusqlite_err(e, "inline.query_all"))
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
        let mut stmt = self
            .conn()
            .prepare(&statement.sql)
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
        let mut total: u64 = 0;
        for statement in &statements {
            let mut stmt = self
                .conn()
                .prepare(&statement.sql)
                .map_err(|e| map_rusqlite_err(e, "inline.execute_batch"))?;
            bind_params(&mut stmt, &statement.params)
                .map_err(|e| map_rusqlite_err(e, "inline.execute_batch"))?;
            total += stmt
                .raw_execute()
                .map_err(|e| map_rusqlite_err(e, "inline.execute_batch"))?
                as u64;
        }
        Ok(total)
    }

    async fn execute_script(&mut self, script: String) -> khive_storage::types::StorageResult<()> {
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
/// ADR-067 Component A, Fork C slice 2 round 2 (HIGH finding): this used to
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
) -> khive_storage::types::StorageResult<Box<dyn Any + Send>> {
    fn tx_stmt(sql: &str, label: &str) -> SqlStatement {
        SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: Some(label.to_string()),
        }
    }
    khive_storage::SqlWriter::execute(writer, tx_stmt("BEGIN IMMEDIATE", "begin")).await?;
    let _tx_handle = khive_storage::tx_registry::register(Some("atomic_unit".to_string()));

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
/// - File-backed: standalone connections per reader/writer call (high
///   concurrency); atomic units drive a single registered raw transaction
///   span instead of a caller-held per-tx connection.
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
            if self.pool.config().read_only {
                return Err(StorageError::Pool {
                    operation: "writer".into(),
                    message: "backend is read-only".into(),
                });
            }
            let conn = open_standalone_writer(&self.pool)?;
            // Best-effort: a lookup failure (no runtime context) degrades to the
            // standalone-connection path in `execute_batch` rather than failing
            // handle construction (mirrors every other migrated store).
            let writer_task = self.pool.writer_task_handle().ok().flatten();
            Ok(Box::new(SqliteWriter {
                conn: Some(conn),
                writer_task,
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
            // falling back to a competing connection from a sync caller.
            if let Some(writer_task) = self.pool.writer_task_handle()? {
                // Flag-on: ONE queued WriteRequest. `run_writer_task` already
                // has an open `BEGIN IMMEDIATE` on its dedicated connection
                // before this closure runs and issues `COMMIT`/`ROLLBACK`
                // after it returns — `op` must not (and, via `InlineWriter`,
                // does not) issue its own transaction control.
                return writer_task
                    .send(move |conn| {
                        let mut inline = InlineWriter {
                            conn: conn as *const rusqlite::Connection,
                        };
                        // Flatten: `block_on_sync` now returns `Result<F::Output,
                        // StorageError>` (outer = "did the future actually
                        // resolve on first poll", inner = the op's own
                        // `StorageResult`) instead of panicking on `Pending`
                        // (HIGH finding, ADR-067 Fork C slice 2 round 2). Either
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
            let conn = open_standalone_writer(&self.pool)?;
            let mut writer = SqliteWriter {
                conn: Some(conn),
                writer_task: None,
            };
            run_manual_atomic_unit(&mut writer, op).await
        } else {
            // In-memory pools are exempt (not accept-loop reachable, per the
            // rework spec's "Out of scope") — preserve the existing
            // pool-backed manual-transaction behavior.
            let mut writer = PoolBackedWriter {
                pool: Arc::clone(&self.pool),
            };
            run_manual_atomic_unit(&mut writer, op).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;
    use khive_storage::types::{SqlStatement, SqlValue};
    use khive_storage::SqlAccess as _;

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
            write_queue_enabled: true,
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
            write_queue_enabled: true,
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

    /// ADR-067 Component A, Fork C slice 2 round 2 (HIGH finding): before
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
    /// `write_queue_enabled: true` in the `PoolConfig` literal, same
    /// technique as this round's other new routing tests.
    #[tokio::test]
    async fn atomic_unit_pending_future_errors_without_killing_writer_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic_unit_pending_future.db");
        let config = PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: true,
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
}
