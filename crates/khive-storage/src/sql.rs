//! SQL access capability traits.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;

use crate::types::{PageRequest, SqlRow, SqlStatement, SqlValue, StorageResult};

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
    /// Implementations must not convert later matching rows into owned values.
    /// Implementations may stop stepping the statement early, so statements with
    /// side effects (e.g. DML with `RETURNING`) must not be issued through this
    /// method; use [`SqlWriter::execute`] or [`SqlWriter::execute_batch`] for writes.
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>>;
    /// Execute `statement` and return all rows.
    ///
    /// This compatibility primitive has no result-size bound. Callers that do
    /// not already constrain their SQL should use [`Self::query_page`].
    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>>;
    /// Execute `statement` and return only the requested offset page.
    ///
    /// The default preserves source compatibility for alternate backends by
    /// slicing [`Self::query_all`]. Backends should override this method when
    /// they can stop row conversion at `page.limit`; khive-db's SQLite bridge
    /// does so. On that path, materialization is bounded by the caller-supplied
    /// `page.limit`, and callers own choosing a sane limit; the database engine
    /// may still do work for the query plan and offset.
    ///
    /// Like [`Self::query_all`], the default has no result-size bound: it
    /// materializes every matching row before slicing. Callers issuing
    /// unconstrained SQL must not rely on the default to keep memory
    /// proportional to `page.limit` — that bound holds only on backends that
    /// override this method.
    ///
    /// Implementations may stop stepping the statement early, so statements with
    /// side effects (e.g. DML with `RETURNING`) must not be issued through this
    /// method; use [`SqlWriter::execute`] or [`SqlWriter::execute_batch`] for writes.
    async fn query_page(
        &mut self,
        statement: SqlStatement,
        page: PageRequest,
    ) -> StorageResult<Vec<SqlRow>> {
        let rows = self.query_all(statement).await?;
        let offset = usize::try_from(page.offset).unwrap_or(usize::MAX);
        let limit = usize::try_from(page.limit).unwrap_or(usize::MAX);
        Ok(rows.into_iter().skip(offset).take(limit).collect())
    }
    /// Execute `statement` and return the first column of the first row as a scalar.
    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>>;
    /// Run `EXPLAIN QUERY PLAN` for `statement` and return the plan rows.
    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>>;
}

/// Write-capable SQL connection (extends `SqlReader`).
#[async_trait]
pub trait SqlWriter: SqlReader + Send + 'static {
    /// Execute a single DML statement and return the number of rows affected.
    ///
    /// Transaction-control rejection is an `execute_batch` contract; this
    /// primitive remains available to internal transaction owners such as
    /// `atomic_unit` for their `BEGIN`/`COMMIT`/`ROLLBACK` calls.
    async fn execute(&mut self, statement: SqlStatement) -> StorageResult<u64>;
    /// Execute multiple DML statements and return the total rows affected.
    async fn execute_batch(&mut self, statements: Vec<SqlStatement>) -> StorageResult<u64>;
    /// Execute a raw SQL script (no parameters; used for migrations).
    ///
    /// This script boundary is internal/migration-only and deliberately does
    /// not inherit `execute_batch`'s transaction-control rejection.
    async fn execute_script(&mut self, script: String) -> StorageResult<()>;

    /// Execute a raw SQL script that MUST run outside any open transaction
    /// (ADR-067 Component A, Fork C slice 2) — e.g.
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
    ///
    /// This is an internal maintenance boundary (for example `VACUUM` or a
    /// checkpoint script), not an `execute_batch` transaction-control guard.
    async fn execute_script_top_level(&mut self, script: String) -> StorageResult<()> {
        self.execute_script(script).await
    }
}

/// Base SQL access capability.
#[async_trait]
pub trait SqlAccess: Send + Sync + 'static {
    /// Acquire a read-only connection from the pool.
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>>;
    /// Acquire a read-write connection from the pool.
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>>;

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
    ///
    /// **The atomic-unit suspend-free invariant (normative for every
    /// caller):** `op`'s future must complete on its **first poll** — it may
    /// issue only synchronous DML against the `&mut dyn SqlWriter` it is
    /// handed and must never reach a real suspension point (no embedding
    /// computation, no ANN warming, no service or channel `await`, no
    /// network round-trip). Synchronous external work is forbidden too: no
    /// filesystem/process/network I/O, sleeps, blocking waits, or unbounded CPU
    /// may run between the transaction owner's `BEGIN IMMEDIATE` and
    /// `COMMIT`. First-poll enforcement alone cannot detect those operations;
    /// ADR-091's write-transaction audit table is the review-time guard, and a
    /// new owner or caller must update that table before merge. On the
    /// single-writer path this is enforced at runtime: the writer task drives
    /// `op` through a single-poll driver and
    /// returns a typed error the instant the future is `Pending`, so a
    /// violation fails loudly rather than corrupting state. On the flag-off
    /// path (no writer task active) a suspending `op` would currently
    /// *succeed* — that path drives `op` as an ordinary `.await` under a
    /// manual transaction — so the invariant is a correctness contract this
    /// trait asks every caller to uphold, not something the type system or
    /// every code path enforces. Callers must not rely on the flag-off
    /// path's tolerance; behavior must be identical (synchronous DML only)
    /// regardless of whether the single-writer flag is on.
    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>>;
}
