//! SQL access capability traits.

use async_trait::async_trait;

use crate::types::{SqlRow, SqlStatement, SqlTxOptions, SqlValue, StorageResult};

/// Read-capable SQL connection.
#[async_trait]
pub trait SqlReader: Send + 'static {
    /// Execute `statement` and return the first row, or `None` if the result set is empty.
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>>;
    /// Execute `statement` and return all rows.
    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>>;
    /// Execute `statement` and return the first column of the first row as a scalar.
    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>>;
    /// Run `EXPLAIN QUERY PLAN` for `statement` and return the plan rows.
    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>>;
}

/// Write-capable SQL connection (extends `SqlReader`).
#[async_trait]
pub trait SqlWriter: SqlReader + Send + 'static {
    /// Execute a single DML statement and return the number of rows affected.
    async fn execute(&mut self, statement: SqlStatement) -> StorageResult<u64>;
    /// Execute multiple DML statements and return the total rows affected.
    async fn execute_batch(&mut self, statements: Vec<SqlStatement>) -> StorageResult<u64>;
    /// Execute a raw SQL script (no parameters; used for migrations).
    async fn execute_script(&mut self, script: String) -> StorageResult<()>;
}

/// A SQL transaction (extends `SqlWriter`).
#[async_trait]
pub trait SqlTransaction: SqlWriter + Send + 'static {
    /// Commit the transaction, persisting all changes.
    async fn commit(self: Box<Self>) -> StorageResult<()>;
    /// Roll back the transaction, discarding all changes.
    async fn rollback(self: Box<Self>) -> StorageResult<()>;
}

/// Base SQL access capability.
#[async_trait]
pub trait SqlAccess: Send + Sync + 'static {
    /// Acquire a read-only connection from the pool.
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>>;
    /// Acquire a read-write connection from the pool.
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>>;
    /// Begin a transaction with the given isolation options.
    async fn begin_tx(&self, options: SqlTxOptions) -> StorageResult<Box<dyn SqlTransaction>>;
}
