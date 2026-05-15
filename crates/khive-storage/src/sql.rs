//! SQL access capability traits.

use async_trait::async_trait;

use crate::types::{SqlRow, SqlStatement, SqlTxOptions, SqlValue, StorageResult};

/// Read-capable SQL connection.
#[async_trait]
pub trait SqlReader: Send + 'static {
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>>;
    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>>;
    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>>;
    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>>;
}

/// Write-capable SQL connection (extends `SqlReader`).
#[async_trait]
pub trait SqlWriter: SqlReader + Send + 'static {
    async fn execute(&mut self, statement: SqlStatement) -> StorageResult<u64>;
    async fn execute_batch(&mut self, statements: Vec<SqlStatement>) -> StorageResult<u64>;
    async fn execute_script(&mut self, script: String) -> StorageResult<()>;
}

/// A SQL transaction (extends `SqlWriter`).
#[async_trait]
pub trait SqlTransaction: SqlWriter + Send + 'static {
    async fn commit(self: Box<Self>) -> StorageResult<()>;
    async fn rollback(self: Box<Self>) -> StorageResult<()>;
}

/// Base SQL access capability.
#[async_trait]
pub trait SqlAccess: Send + Sync + 'static {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>>;
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>>;
    async fn begin_tx(&self, options: SqlTxOptions) -> StorageResult<Box<dyn SqlTransaction>>;
}
