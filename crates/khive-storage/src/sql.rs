//! SQL access capability traits.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;

use crate::types::{SqlRow, SqlStatement, SqlTxOptions, SqlValue, StorageResult};

/// A boxed future, borrowing from the `&mut dyn SqlWriter` an
/// [`AtomicUnitOp`] is called with (see [`SqlAccess::atomic_unit`]).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A caller-supplied unit of work to run as ONE atomic operation via
/// [`SqlAccess::atomic_unit`] (ADR-067 Component A, Fork C slice 2).
///
/// `op` receives a live `&mut dyn SqlWriter` already inside an open write
/// transaction — it must issue DML only (no bare `BEGIN`/`COMMIT`/
/// `ROLLBACK`; the caller-visible transaction boundary is owned entirely by
/// `atomic_unit`, exactly like the existing `execute_batch` contract) — and
/// returns its result type-erased via `Box<dyn Any + Send>` so this trait
/// method stays object-safe (no method-level generics on a trait used as
/// `dyn SqlAccess`). Callers downcast the returned box back to their own
/// concrete outcome type.
pub type AtomicUnitOp = Box<
    dyn for<'w> FnOnce(&'w mut dyn SqlWriter) -> BoxFuture<'w, StorageResult<Box<dyn Any + Send>>>
        + Send,
>;

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

    /// Execute a raw SQL script that MUST run outside any open transaction
    /// (ADR-067 Component A, Fork C slice 2 round 2, BLOCKER A) — e.g.
    /// `VACUUM`, which SQLite rejects if issued inside `BEGIN`/`COMMIT`.
    ///
    /// Default implementation delegates to [`Self::execute_script`]: every
    /// writer implementation in this codebase except khive-db's
    /// write-queue-routed `SqliteWriter` already runs `execute_script`
    /// transaction-free (a plain connection call, or already inside a
    /// caller-managed transaction where a top-level statement would be
    /// invalid regardless of which method is called). `SqliteWriter`
    /// overrides this to route around its writer task's per-request `BEGIN
    /// IMMEDIATE` specifically for this call, while still serializing
    /// through the single writer owner.
    async fn execute_script_top_level(&mut self, script: String) -> StorageResult<()> {
        self.execute_script(script).await
    }
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

    /// Run `op` as ONE atomic unit of work (ADR-067 Component A, Fork C
    /// slice 2).
    ///
    /// Where a single-writer task is active (file-backed pool,
    /// `KHIVE_WRITE_QUEUE=1`), `op` runs inside that task's one write
    /// transaction for this request — no separate connection is opened, so
    /// this call cannot compete with the writer task for SQLite's write
    /// lock. Where no writer task applies (flag off, no runtime, or an
    /// in-memory pool), `op` runs under a manual
    /// `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` on a writer handle exactly like
    /// calling [`Self::writer`] and driving the statements by hand — the
    /// pre-ADR-067 behavior, preserved byte-for-byte on this path.
    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>>;
}
